//! Serve-side reasoning bridge (ADR-0042 Slice 3).
//!
//! When a pipeline exposed via `zymi mcp serve` hits an `ask:` step, the run
//! parks on a [`EventKind::ReasoningRequested`]. Under serve there is no MCP
//! primitive that makes the *caller's model* reason on demand (sampling is
//! deprecated, SEP-2577; elicitation returns human form data). So instead of
//! answering inline, this bridge returns control to the caller's own agent
//! loop: it flips the owning task to `InputRequired`, carrying the prompt and
//! an opaque [`ResumeTokenSigner`]-signed `resume_token`. The caller reasons,
//! then calls the `zymi/reasoning/resume` method with the token + its answer,
//! which publishes [`EventKind::ReasoningAnswered`] and the parked run
//! continues. This is the async task + resume surface (ADR-0033 2a), plain
//! stateless request/response — no `sampling`, no `elicitation`.
//!
//! **Token design (ADR-0042 *Alignment with SEP-2322*):** the `resume_token`
//! is opaque, integrity-protected (HMAC-SHA256 over a per-process random key),
//! and carries an expiry. It is deliberately **not** shaped against the
//! unreleased SEP-2322 `requestState`; MRTR is a *future* migration target,
//! not a v1 constraint. The token is signed with `sha2` only — `hmac`/`hex`/
//! `subtle` are behind the `connectors` feature but this module compiles under
//! bare `runtime`, so it hand-rolls a small HMAC/hex/constant-time-eq.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::approval::ChannelHandle;
use crate::events::bus::EventBus;
use crate::events::{Event, EventKind};
use crate::reasoning::ReasoningChannel;

use super::protocol::TaskStatus;
use super::tasks::TaskStore;

/// The reasoning channel name serve routes `ask:` steps to. Distinct from the
/// elicitation approval channel ([`super::elicitation::CHANNEL_NAME`]).
pub const CHANNEL_NAME: &str = "mcp_reasoning";

/// Default lifetime of a `resume_token` — the parked `ask:` fails closed after
/// this (ADR-0042 Decision §4). Matches the approval timeout default.
pub const DEFAULT_RESUME_TTL: Duration = crate::approval::DEFAULT_APPROVAL_TIMEOUT;

// ---------------------------------------------------------------------------
// Resume token
// ---------------------------------------------------------------------------

/// Verified claims decoded from a `resume_token`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeClaims {
    pub request_id: String,
    pub stream_id: String,
    pub correlation: Uuid,
}

/// Why a `resume_token` was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// Structurally invalid (bad hex, missing separator, wrong field count).
    Malformed,
    /// HMAC did not match — forged or key mismatch.
    BadSignature,
    /// Past its expiry (ADR-0042 Decision §4 — fail closed).
    Expired,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Malformed => write!(f, "malformed resume_token"),
            TokenError::BadSignature => write!(f, "resume_token signature mismatch"),
            TokenError::Expired => write!(f, "resume_token expired"),
        }
    }
}

/// Mints and verifies opaque, integrity-protected `resume_token`s. The key is
/// random per process (tasks live only for the serve session), so tokens are
/// unforgeable and need no persistence.
pub struct ResumeTokenSigner {
    key: [u8; 32],
}

impl Default for ResumeTokenSigner {
    fn default() -> Self {
        Self::random()
    }
}

impl ResumeTokenSigner {
    /// A signer with a fresh random key (32 bytes from the OS RNG via UUIDv4).
    pub fn random() -> Self {
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        key[16..].copy_from_slice(Uuid::new_v4().as_bytes());
        Self { key }
    }

    /// Mint a token binding `request_id` + `stream_id` + `correlation` with an
    /// expiry `ttl` from now. The token is `hex(payload).hex(hmac)`; the
    /// payload is `request_id|stream_id|correlation|exp_unix` (all fields are
    /// UUIDs / a `mcp-task-*` id / an integer, none contain `|`).
    pub fn mint(
        &self,
        request_id: &str,
        stream_id: &str,
        correlation: Uuid,
        ttl: Duration,
    ) -> String {
        let exp = Utc::now().timestamp()
            + chrono::Duration::from_std(ttl)
                .map(|d| d.num_seconds())
                .unwrap_or(300);
        let payload = format!("{request_id}|{stream_id}|{correlation}|{exp}");
        let sig = hmac_sha256(&self.key, payload.as_bytes());
        format!("{}.{}", to_hex(payload.as_bytes()), to_hex(&sig))
    }

    /// Verify integrity + expiry and return the claims. Never trusts the
    /// caller-supplied payload before the signature checks out.
    pub fn verify(&self, token: &str) -> Result<ResumeClaims, TokenError> {
        let (payload_hex, sig_hex) = token.split_once('.').ok_or(TokenError::Malformed)?;
        let payload_bytes = from_hex(payload_hex).ok_or(TokenError::Malformed)?;
        let sig = from_hex(sig_hex).ok_or(TokenError::Malformed)?;
        let expected = hmac_sha256(&self.key, &payload_bytes);
        if !ct_eq(&sig, &expected) {
            return Err(TokenError::BadSignature);
        }
        // Signature is valid — the payload is ours, safe to parse.
        let payload = String::from_utf8(payload_bytes).map_err(|_| TokenError::Malformed)?;
        let parts: Vec<&str> = payload.split('|').collect();
        if parts.len() != 4 {
            return Err(TokenError::Malformed);
        }
        let exp: i64 = parts[3].parse().map_err(|_| TokenError::Malformed)?;
        if Utc::now().timestamp() > exp {
            return Err(TokenError::Expired);
        }
        let correlation = Uuid::parse_str(parts[2]).map_err(|_| TokenError::Malformed)?;
        Ok(ResumeClaims {
            request_id: parts[0].to_string(),
            stream_id: parts[1].to_string(),
            correlation,
        })
    }
}

/// HMAC-SHA256 (RFC 2104) over `sha2` only, so this module needs no
/// `connectors`-gated crypto crate.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_h = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_h);
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer.finalize());
    out
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Constant-time byte-slice equality (length-independent short-circuit only).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Serve reasoning channel
// ---------------------------------------------------------------------------

/// The `{ status: "needs_reasoning", prompt, resume_token }` payload surfaced
/// to the caller on the parked task. `tasks/get` merges this in when the task
/// is `InputRequired`.
pub fn needs_reasoning_payload(prompt: &str, resume_token: &str) -> serde_json::Value {
    serde_json::json!({
        "status": "needs_reasoning",
        "prompt": prompt,
        "resume_token": resume_token,
    })
}

/// A [`ReasoningChannel`] that bridges a parked `ask:` step to the connected
/// caller's agent loop via the task/resume surface (ADR-0042 Decision §3).
///
/// Unlike [`crate::reasoning::TerminalReasoningChannel`], it does **not**
/// answer the request — it flips the owning task to `InputRequired` and lets
/// the caller supply the answer through `zymi/reasoning/resume`.
pub struct McpTaskReasoningChannel {
    name: String,
    store: Arc<TaskStore>,
    signer: Arc<ResumeTokenSigner>,
    ttl: Duration,
}

impl McpTaskReasoningChannel {
    pub fn new(store: Arc<TaskStore>, signer: Arc<ResumeTokenSigner>) -> Self {
        Self {
            name: CHANNEL_NAME.to_string(),
            store,
            signer,
            ttl: DEFAULT_RESUME_TTL,
        }
    }
}

#[async_trait]
impl ReasoningChannel for McpTaskReasoningChannel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn start(&self, bus: Arc<EventBus>) -> Result<ChannelHandle, String> {
        let mut rx = bus.subscribe().await;
        let channel_name = self.name.clone();
        let store = Arc::clone(&self.store);
        let signer = Arc::clone(&self.signer);
        let ttl = self.ttl;

        let join = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let EventKind::ReasoningRequested {
                    request_id,
                    stream_id,
                    prompt,
                    channel,
                } = &ev.kind
                else {
                    continue;
                };
                if channel != &channel_name {
                    continue;
                }
                let correlation = match ev.correlation_id {
                    Some(c) => c,
                    // No correlation ⇒ the run's await could never match an
                    // answer; nothing useful to bridge.
                    None => continue,
                };

                // The run parks on its own stream; find the owning task.
                let Some(task) = store.find_by_stream_id(stream_id).await else {
                    log::warn!(
                        "mcp reasoning: no task for stream '{stream_id}' — \
                         request '{request_id}' cannot be surfaced (sync tools/call \
                         cannot answer reasoning; task-augment the call)"
                    );
                    continue;
                };

                let token = signer.mint(request_id, stream_id, correlation, ttl);
                let payload = needs_reasoning_payload(prompt, &token);

                let mut s = task.state.lock().await;
                if s.status.is_terminal() {
                    continue;
                }
                s.status = TaskStatus::InputRequired;
                s.status_message = Some(prompt.clone());
                s.result = Some(payload);
                s.last_updated_at = Utc::now();
            }
        });

        Ok(ChannelHandle::from_task(self.name.clone(), join))
    }
}

/// Handle a `zymi/reasoning/resume` call: verify the token, publish the
/// caller's answer as [`EventKind::ReasoningAnswered`] so the parked run
/// continues, and flip the owning task back to `Working`.
///
/// The answer is **untrusted, model-generated free text** — it re-enters zymi
/// as an `ask:` step output and MUST pass the tool-output guard discipline at
/// any sink (ADR-0042 Security). It is not sanitized here; the sink guards
/// (ADR-0039/0036) apply provenance-agnostically.
pub async fn resume(
    bus: &EventBus,
    store: &TaskStore,
    signer: &ResumeTokenSigner,
    token: &str,
    answer: &str,
) -> Result<ResumeClaims, TokenError> {
    let claims = signer.verify(token)?;

    let answered = Event::new(
        claims.stream_id.clone(),
        EventKind::ReasoningAnswered {
            request_id: claims.request_id.clone(),
            stream_id: claims.stream_id.clone(),
            answer: answer.to_string(),
            answered_by: format!("mcp_reasoning:{}", claims.stream_id),
            is_error: false,
        },
        "reasoning_channel".into(),
    )
    .with_correlation(claims.correlation);
    if let Err(e) = bus.publish(answered).await {
        log::warn!("mcp reasoning resume publish failed: {e}");
    }

    // Best-effort flip back to Working; the run's completion wrapper takes it
    // terminal. If the run already parked on the *next* ask, its channel
    // handler will re-flip to InputRequired.
    if let Some(task) = store.find_by_stream_id(&claims.stream_id).await {
        let mut s = task.state.lock().await;
        if s.status == TaskStatus::InputRequired {
            s.status = TaskStatus::Working;
            s.status_message = None;
            s.result = None;
            s.last_updated_at = Utc::now();
        }
    }

    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trips() {
        let signer = ResumeTokenSigner::random();
        let corr = Uuid::new_v4();
        let token = signer.mint("req-1", "mcp-task-abc", corr, Duration::from_secs(60));
        let claims = signer.verify(&token).unwrap();
        assert_eq!(claims.request_id, "req-1");
        assert_eq!(claims.stream_id, "mcp-task-abc");
        assert_eq!(claims.correlation, corr);
    }

    #[test]
    fn tampered_token_is_rejected() {
        let signer = ResumeTokenSigner::random();
        let token = signer.mint("req-1", "s-1", Uuid::new_v4(), Duration::from_secs(60));
        // Flip a nibble in the signature half.
        let (payload, sig) = token.split_once('.').unwrap();
        let mut sig: Vec<char> = sig.chars().collect();
        sig[0] = if sig[0] == 'a' { 'b' } else { 'a' };
        let forged = format!("{payload}.{}", sig.into_iter().collect::<String>());
        assert_eq!(signer.verify(&forged), Err(TokenError::BadSignature));
    }

    #[test]
    fn foreign_key_signature_is_rejected() {
        let a = ResumeTokenSigner::random();
        let b = ResumeTokenSigner::random();
        let token = a.mint("req-1", "s-1", Uuid::new_v4(), Duration::from_secs(60));
        assert_eq!(b.verify(&token), Err(TokenError::BadSignature));
    }

    #[test]
    fn expired_token_is_rejected() {
        let signer = ResumeTokenSigner::random();
        // Mint already-expired (ttl 0 ⇒ exp == now, and now > exp on the next tick).
        let token = signer.mint("req-1", "s-1", Uuid::new_v4(), Duration::from_secs(0));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(signer.verify(&token), Err(TokenError::Expired));
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        let signer = ResumeTokenSigner::random();
        assert_eq!(signer.verify("no-dot"), Err(TokenError::Malformed));
        assert_eq!(signer.verify("zz.zz"), Err(TokenError::Malformed));
        assert!(signer.verify(".").is_err());
    }
}
