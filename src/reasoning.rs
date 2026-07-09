//! Reasoning delegation (ADR-0042): the `ask:` step machinery.
//!
//! An `ask:` step parks the run, surfaces a prompt to the connected caller as
//! a *reasoning request*, and resumes with the caller's text answer as the
//! step output. It is the approval mechanism (ADR-0022) generalized from a
//! *decision* (approve/deny) to a *value* (free text in, free text out).
//!
//! The orchestrator's only contract is the bus: it publishes
//! [`EventKind::ReasoningRequested`] and awaits a matching
//! [`EventKind::ReasoningAnswered`], exactly as the approval path publishes
//! `ApprovalRequested` and awaits `ApprovalGranted` / `ApprovalDenied`. A
//! `ReasoningChannel` (Slice 2) consumes the request and answers it — from the
//! calling agent's model under `zymi mcp serve`, from a human on a manual
//! channel, or from a configured service.
//!
//! **Security:** a reasoning answer is *untrusted, model-generated free text*
//! over often attacker-influenced context. It carries the same taint as any
//! tool output and MUST pass the tool-output guard discipline (ADR-0036/0039)
//! before reaching a sink. See the ADR-0042 Security section.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use crate::approval::ChannelHandle;
use crate::events::bus::EventBus;
use crate::events::{Event, EventKind};

/// Outcome of a reasoning round-trip.
pub enum ReasoningOutcome {
    /// The caller answered. Carries the untrusted answer text.
    Answered(String),
    /// The request failed closed (timeout, bus closed, or a synthesised
    /// error answer). Carries a human-readable reason; the audit trail
    /// already holds a `ReasoningAnswered { is_error: true }`.
    Failed(String),
}

/// Publish [`EventKind::ReasoningRequested`] on `bus` with the given channel
/// routing, then await a matching [`EventKind::ReasoningAnswered`] (filtered
/// by `correlation_id`) until `timeout` elapses.
///
/// On timeout or bus closure, synthesises a
/// `ReasoningAnswered { is_error: true, answer: "timeout" }` so the audit
/// trail records the fail-closed outcome, and returns
/// [`ReasoningOutcome::Failed`]. This mirrors
/// [`crate::approval::request_approval_via_bus`] — the caller is the
/// orchestrator; channel plugins on the bus drive the answering surface.
pub async fn request_reasoning_via_bus(
    bus: &EventBus,
    stream_id: &str,
    correlation_id: Uuid,
    channel: &str,
    prompt: &str,
    timeout: Duration,
) -> ReasoningOutcome {
    let request_id = Uuid::new_v4().to_string();

    // Subscribe BEFORE publishing — a synchronous channel could otherwise
    // answer before we attach.
    let mut rx = bus.subscribe_correlation(correlation_id).await;

    let request = Event::new(
        stream_id.to_string(),
        EventKind::ReasoningRequested {
            request_id: request_id.clone(),
            stream_id: stream_id.to_string(),
            prompt: prompt.to_string(),
            channel: channel.to_string(),
        },
        "orchestrator".into(),
    )
    .with_correlation(correlation_id);

    if let Err(e) = bus.publish(request).await {
        log::warn!("reasoning request publish failed: {e}");
        return ReasoningOutcome::Failed(format!("reasoning request publish failed: {e}"));
    }

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => match &ev.kind {
                EventKind::ReasoningAnswered {
                    request_id: rid,
                    answer,
                    is_error,
                    ..
                } if rid == &request_id => {
                    if *is_error {
                        return ReasoningOutcome::Failed(answer.clone());
                    }
                    return ReasoningOutcome::Answered(answer.clone());
                }
                _ => continue,
            },
            // bus subscription closed
            Ok(None) => break,
            // outer timeout fired
            Err(_) => break,
        }
    }

    // Timed out or bus closed. Seal the audit trail with a synthetic error
    // answer, mirroring the approval path's synthesised denial.
    synthesize_failure(bus, stream_id, &request_id, correlation_id, "timeout").await;
    ReasoningOutcome::Failed("reasoning request timed out".into())
}

/// Publish a fail-closed [`EventKind::ReasoningAnswered`] so an unanswerable
/// or abandoned request closes in the audit trail. Used both by the timeout
/// path above and by the orchestrator when no channel can answer at all.
pub async fn synthesize_failure(
    bus: &EventBus,
    stream_id: &str,
    request_id: &str,
    correlation_id: Uuid,
    reason: &str,
) {
    let sealed = Event::new(
        stream_id.to_string(),
        EventKind::ReasoningAnswered {
            request_id: request_id.to_string(),
            stream_id: stream_id.to_string(),
            answer: reason.to_string(),
            answered_by: "orchestrator".into(),
            is_error: true,
        },
        "orchestrator".into(),
    )
    .with_correlation(correlation_id);
    if let Err(e) = bus.publish(sealed).await {
        log::warn!("reasoning failure publish failed: {e}");
    }
}

/// Resolve which reasoning channel answers an `ask:` step (ADR-0042):
///
/// 1. Step-level `channel:` override (the `ask:`'s own field).
/// 2. Runtime default (`Runtime::reasoning_channel`), which under `zymi run`
///    is the zero-config `"terminal"` and under `zymi mcp serve` is the
///    connected caller (Slice 3).
/// 3. `None` → the orchestrator fail-closes (Decision §5). No silent fallback
///    to an HTTP provider — the whole point is *not* reaching for the API.
pub fn resolve_reasoning_channel(
    step_override: Option<&str>,
    runtime_default: Option<&str>,
) -> Option<String> {
    step_override
        .map(str::to_string)
        .or_else(|| runtime_default.map(str::to_string))
}

/// A reasoning channel plugin (ADR-0042). The sibling of
/// [`crate::approval::ApprovalChannel`]: channels subscribe to the event bus,
/// filter [`EventKind::ReasoningRequested`] by their `name`, obtain an answer
/// (from a human prompt, a service, or — under `serve` — the calling agent's
/// model), and publish [`EventKind::ReasoningAnswered`] back with the same
/// `request_id`. The orchestrator only ever talks to the bus.
///
/// Where an [`ApprovalChannel`](crate::approval::ApprovalChannel) returns a
/// *decision*, a `ReasoningChannel` returns a *value*.
#[async_trait]
pub trait ReasoningChannel: Send + Sync {
    /// Channel name, matched against [`EventKind::ReasoningRequested::channel`].
    fn name(&self) -> &str;
    /// Spawn the channel's bus listener. Must subscribe before returning so a
    /// request published immediately after start is not missed.
    async fn start(&self, bus: Arc<EventBus>) -> Result<ChannelHandle, String>;
}

/// Terminal-driven [`ReasoningChannel`] (ADR-0042). Subscribes to the bus,
/// picks up [`EventKind::ReasoningRequested`] addressed to this channel,
/// prompts on stdin for a free-text answer (serialised across requests so
/// concurrent steps don't garble the prompt), and publishes
/// [`EventKind::ReasoningAnswered`]. EOF / IO error seals the request with an
/// `is_error: true` answer so the run fails closed rather than hanging.
///
/// This is the zero-config answerer for `ask:` under `zymi run` from an
/// interactive shell — the reasoning analogue of
/// [`crate::approval::TerminalApprovalChannel`].
pub struct TerminalReasoningChannel {
    name: String,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl TerminalReasoningChannel {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

#[async_trait]
impl ReasoningChannel for TerminalReasoningChannel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn start(&self, bus: Arc<EventBus>) -> Result<ChannelHandle, String> {
        let mut rx = bus.subscribe().await;
        let channel_name = self.name.clone();
        let lock = Arc::clone(&self.lock);

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

                let prompt = prompt.clone();
                let request_id = request_id.clone();
                let stream_id_owned = stream_id.clone();
                let correlation_id = ev.correlation_id;
                let lock = Arc::clone(&lock);
                let bus = Arc::clone(&bus);
                let channel_name_owned = channel_name.clone();

                // Each request on its own task so a slow human typing can't
                // pin the bus subscriber.
                tokio::spawn(async move {
                    let _guard = lock.lock().await;
                    let answer = tokio::task::spawn_blocking(move || {
                        terminal_reasoning_prompt_blocking(&prompt)
                    })
                    .await
                    .unwrap_or(None);

                    let kind = match answer {
                        Some(text) => EventKind::ReasoningAnswered {
                            request_id,
                            stream_id: stream_id_owned.clone(),
                            answer: text,
                            answered_by: format!("terminal:{channel_name_owned}"),
                            is_error: false,
                        },
                        // EOF / IO error — fail closed.
                        None => EventKind::ReasoningAnswered {
                            request_id,
                            stream_id: stream_id_owned.clone(),
                            answer: "no answer (stdin closed)".into(),
                            answered_by: format!("terminal:{channel_name_owned}"),
                            is_error: true,
                        },
                    };
                    let mut answer_event =
                        Event::new(stream_id_owned, kind, "reasoning_channel".into());
                    if let Some(corr) = correlation_id {
                        answer_event = answer_event.with_correlation(corr);
                    }
                    if let Err(e) = bus.publish(answer_event).await {
                        log::warn!("terminal reasoning answer publish failed: {e}");
                    }
                });
            }
        });

        Ok(ChannelHandle::from_task(self.name.clone(), join))
    }
}

fn terminal_reasoning_prompt_blocking(prompt: &str) -> Option<String> {
    use std::io::{self, BufRead, Write};

    {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        writeln!(out).ok();
        writeln!(out, "--- reasoning requested -------------------------").ok();
        writeln!(out, "  {prompt}").ok();
        write!(out, "  answer> ").ok();
        out.flush().ok();
    }

    let stdin = io::stdin();
    let mut line = String::new();
    match stdin.lock().read_line(&mut line) {
        Ok(0) => None, // EOF
        Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_string()),
        Err(_) => None,
    }
}

/// Outcome of [`replay_unfulfilled_reasoning`].
#[derive(Default, Debug)]
pub struct ReasoningReplayReport {
    /// Requests whose age exceeded `timeout`; sealed with a synthesised
    /// `ReasoningAnswered { is_error: true, answer: "restart_timeout" }`.
    pub expired: Vec<String>,
    /// Requests still within their timeout window; redelivered on the bus so
    /// live channel listeners can re-prompt.
    pub redelivered: Vec<String>,
}

/// Walk the event store and reconcile any `ReasoningRequested` whose matching
/// `ReasoningAnswered` is missing — the startup-replay path for `ask:` steps,
/// mirroring [`crate::approval::replay_unfulfilled_approvals`]. After a hard
/// crash the orchestrator's await loop is gone but the request row persists;
/// without this the audit trail leaves the request open forever and live
/// channels never see it.
///
/// Expired (age ≥ `timeout`) → seal with a synthetic error answer so the
/// audit trail closes. In-flight → [`EventBus::redeliver`] (no re-append).
pub async fn replay_unfulfilled_reasoning(
    bus: Arc<EventBus>,
    timeout: Duration,
) -> Result<ReasoningReplayReport, String> {
    use chrono::Utc;
    use std::collections::{HashMap, HashSet};

    let store = Arc::clone(bus.store());

    let mut pending: HashMap<String, Event> = HashMap::new();
    let mut answered: HashSet<String> = HashSet::new();

    let batch = 1024usize;
    let mut from = 0u64;
    loop {
        let tailed = store
            .tail(from, batch)
            .await
            .map_err(|e| format!("reasoning replay: tail: {e}"))?;
        if tailed.is_empty() {
            break;
        }
        for t in &tailed {
            match &t.event.kind {
                EventKind::ReasoningRequested { request_id, .. } => {
                    pending.insert(request_id.clone(), t.event.clone());
                }
                EventKind::ReasoningAnswered { request_id, .. } => {
                    answered.insert(request_id.clone());
                }
                _ => {}
            }
        }
        let last = tailed.last().expect("non-empty checked above").global_seq;
        from = last;
        if tailed.len() < batch {
            break;
        }
    }

    pending.retain(|id, _| !answered.contains(id));
    if pending.is_empty() {
        return Ok(ReasoningReplayReport::default());
    }

    let now = Utc::now();
    let timeout_chrono =
        chrono::Duration::from_std(timeout).unwrap_or_else(|_| chrono::Duration::seconds(300));

    let mut report = ReasoningReplayReport::default();
    for (request_id, ev) in pending {
        let age = now.signed_duration_since(ev.timestamp);
        if age >= timeout_chrono {
            let stream_id = match &ev.kind {
                EventKind::ReasoningRequested { stream_id, .. } => stream_id.clone(),
                _ => unreachable!("pending only holds ReasoningRequested"),
            };
            let correlation = ev.correlation_id.unwrap_or_else(Uuid::new_v4);
            synthesize_failure(&bus, &stream_id, &request_id, correlation, "restart_timeout").await;
            report.expired.push(request_id);
        } else {
            bus.redeliver(Arc::new(ev)).await;
            report.redelivered.push(request_id);
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::store::SqliteEventStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// A live answerer that publishes a real answer keyed by request_id makes
    /// the round-trip return `Answered`, even with unrelated events in between.
    #[tokio::test]
    async fn request_returns_answer_on_reasoning_answered() {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(SqliteEventStore::new(&dir.path().join("reason_ok.db")).unwrap());
        let bus = Arc::new(EventBus::new(store));

        let stream_id = "stream-1".to_string();
        let correlation_id = Uuid::new_v4();
        let bus_for_listener = Arc::clone(&bus);
        let listener = tokio::spawn(async move {
            let mut rx = bus_for_listener.subscribe().await;
            while let Some(ev) = rx.recv().await {
                if let EventKind::ReasoningRequested {
                    request_id,
                    stream_id,
                    ..
                } = &ev.kind
                {
                    let mut answer = Event::new(
                        stream_id.clone(),
                        EventKind::ReasoningAnswered {
                            request_id: request_id.clone(),
                            stream_id: stream_id.clone(),
                            answer: "two sentences.".into(),
                            answered_by: "test".into(),
                            is_error: false,
                        },
                        "reasoning_channel".into(),
                    );
                    if let Some(corr) = ev.correlation_id {
                        answer = answer.with_correlation(corr);
                    }
                    bus_for_listener.publish(answer).await.unwrap();
                    break;
                }
            }
        });

        let outcome = request_reasoning_via_bus(
            bus.as_ref(),
            &stream_id,
            correlation_id,
            "mcp_reasoning",
            "summarize this",
            Duration::from_secs(2),
        )
        .await;
        match outcome {
            ReasoningOutcome::Answered(a) => assert_eq!(a, "two sentences."),
            ReasoningOutcome::Failed(e) => panic!("expected answer, got failure: {e}"),
        }
        listener.await.unwrap();
    }

    /// With no answerer, the request times out and fails closed, and a
    /// synthetic `ReasoningAnswered { is_error: true }` is left in the store.
    #[tokio::test]
    async fn request_fails_closed_on_timeout() {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(SqliteEventStore::new(&dir.path().join("reason_to.db")).unwrap());
        let bus = Arc::new(EventBus::new(store));

        let outcome = request_reasoning_via_bus(
            bus.as_ref(),
            "stream-x",
            Uuid::new_v4(),
            "nobody",
            "summarize",
            Duration::from_millis(50),
        )
        .await;
        assert!(matches!(outcome, ReasoningOutcome::Failed(_)));

        // Synthetic error answer is in the store.
        let mut from = 0u64;
        let mut found = false;
        loop {
            let batch = bus.store().tail(from, 256).await.unwrap();
            if batch.is_empty() {
                break;
            }
            for t in &batch {
                if let EventKind::ReasoningAnswered { is_error, .. } = &t.event.kind {
                    if *is_error {
                        found = true;
                    }
                }
            }
            from = batch.last().unwrap().global_seq;
            if batch.len() < 256 {
                break;
            }
        }
        assert!(found, "expected synthetic ReasoningAnswered{{is_error}} in store");
    }

    #[test]
    fn resolve_channel_step_override_wins() {
        assert_eq!(
            resolve_reasoning_channel(Some("mcp_reasoning"), Some("terminal")).as_deref(),
            Some("mcp_reasoning")
        );
    }

    #[test]
    fn resolve_channel_falls_back_to_runtime_default() {
        assert_eq!(
            resolve_reasoning_channel(None, Some("terminal")).as_deref(),
            Some("terminal")
        );
    }

    #[test]
    fn resolve_channel_none_when_neither_set() {
        assert!(resolve_reasoning_channel(None, None).is_none());
    }

    /// A `ReasoningRequested` older than the timeout is sealed with a
    /// synthetic error answer so the audit trail closes across a restart.
    #[tokio::test]
    async fn replay_seals_expired_request() {
        let dir = TempDir::new().unwrap();
        let store =
            Arc::new(SqliteEventStore::new(&dir.path().join("reason_replay_exp.db")).unwrap());
        let bus = Arc::new(EventBus::new(store));

        let mut stale = Event::new(
            "stream-A".into(),
            EventKind::ReasoningRequested {
                request_id: "old-1".into(),
                stream_id: "stream-A".into(),
                prompt: "summarize".into(),
                channel: "terminal".into(),
            },
            "orchestrator".into(),
        );
        stale.timestamp = chrono::Utc::now() - chrono::Duration::seconds(3600);
        bus.store().append(&mut stale).await.unwrap();

        let report = replay_unfulfilled_reasoning(Arc::clone(&bus), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(report.expired, vec!["old-1".to_string()]);
        assert!(report.redelivered.is_empty());

        // A sealing ReasoningAnswered{is_error} now exists for old-1.
        let mut from = 0u64;
        let mut found = false;
        loop {
            let batch = bus.store().tail(from, 256).await.unwrap();
            if batch.is_empty() {
                break;
            }
            for t in &batch {
                if let EventKind::ReasoningAnswered {
                    request_id,
                    is_error,
                    ..
                } = &t.event.kind
                {
                    if request_id == "old-1" && *is_error {
                        found = true;
                    }
                }
            }
            from = batch.last().unwrap().global_seq;
            if batch.len() < 256 {
                break;
            }
        }
        assert!(found, "expected sealing ReasoningAnswered{{is_error}} for old-1");
    }

    /// An already-answered request is ignored by replay.
    #[tokio::test]
    async fn replay_ignores_answered_request() {
        let dir = TempDir::new().unwrap();
        let store =
            Arc::new(SqliteEventStore::new(&dir.path().join("reason_replay_done.db")).unwrap());
        let bus = Arc::new(EventBus::new(store));

        bus.publish(Event::new(
            "stream-C".into(),
            EventKind::ReasoningRequested {
                request_id: "done-1".into(),
                stream_id: "stream-C".into(),
                prompt: "x".into(),
                channel: "terminal".into(),
            },
            "orchestrator".into(),
        ))
        .await
        .unwrap();
        bus.publish(Event::new(
            "stream-C".into(),
            EventKind::ReasoningAnswered {
                request_id: "done-1".into(),
                stream_id: "stream-C".into(),
                answer: "the answer".into(),
                answered_by: "terminal".into(),
                is_error: false,
            },
            "reasoning_channel".into(),
        ))
        .await
        .unwrap();

        let report = replay_unfulfilled_reasoning(Arc::clone(&bus), Duration::from_secs(3600))
            .await
            .unwrap();
        assert!(report.expired.is_empty());
        assert!(report.redelivered.is_empty());
    }
}
