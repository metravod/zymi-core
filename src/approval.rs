use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_yml::Value as YamlValue;
use uuid::Uuid;

use crate::events::bus::EventBus;
use crate::events::{Event, EventKind};
use crate::plugin::{PluginBuildError, PluginBuilder, PluginRegistry};

/// Default per-request timeout when no explicit value is configured.
/// Five minutes is a deliberate pessimism: enough for an operator to
/// switch terminals or read a Telegram message, short enough that a
/// forgotten approval doesn't pin a stream forever.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// An approval channel plugin (ADR-0022). Channels subscribe to the
/// event bus, filter [`EventKind::ApprovalRequested`] by their `name`,
/// surface the request to a human (terminal prompt, HTTP endpoint,
/// Telegram message, …), and publish [`EventKind::ApprovalGranted`] /
/// [`EventKind::ApprovalDenied`] back onto the bus with the same
/// `approval_id`. The orchestrator never talks to a channel directly —
/// its only contract is the bus.
#[async_trait]
pub trait ApprovalChannel: Send + Sync {
    /// Channel name, matched against [`EventKind::ApprovalRequested::channel`].
    fn name(&self) -> &str;
    /// Spawn the channel's bus listener. Implementations must subscribe
    /// before returning so a request published immediately after start
    /// is not missed. Returns a [`ChannelHandle`] used for shutdown.
    ///
    /// Takes `&self` so channels can be dispatched as
    /// `Box<dyn ApprovalChannel>` from the YAML plugin registry.
    /// Implementations must clone any state they need into spawned
    /// tasks (the `Box` is typically dropped after `start` returns).
    async fn start(&self, bus: Arc<EventBus>) -> Result<ChannelHandle, String>;
}

/// Handle to a running [`ApprovalChannel`]. Holds every tokio task the
/// channel spawned (bus listener, optional axum server, etc.) so
/// [`ChannelHandle::shutdown`] can abort all of them in one call.
///
/// Shutdown is implemented via [`tokio::task::JoinHandle::abort`] —
/// approval channels are I/O-bounded loops with no cleanup beyond
/// dropping their bus subscriber, so abort is safe and avoids pulling
/// `tokio-util` (which is gated behind the `connectors` feature) into
/// the approval surface.
pub struct ChannelHandle {
    pub name: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ChannelHandle {
    pub fn from_task(name: impl Into<String>, task: tokio::task::JoinHandle<()>) -> Self {
        Self {
            name: name.into(),
            tasks: vec![task],
        }
    }

    pub fn from_tasks(
        name: impl Into<String>,
        tasks: Vec<tokio::task::JoinHandle<()>>,
    ) -> Self {
        Self {
            name: name.into(),
            tasks,
        }
    }

    /// Abort every task without awaiting. Suitable for synchronous
    /// drop paths (see [`crate::python::py_runtime::PyRuntime::drop`]).
    pub fn abort_all(&self) {
        for t in &self.tasks {
            t.abort();
        }
    }

    /// Abort every task and await all join handles. Use this from
    /// `async` shutdown paths (CLI entrypoints) so the bus subscriber
    /// drops cleanly before process exit.
    pub async fn shutdown(self) {
        for t in &self.tasks {
            t.abort();
        }
        for t in self.tasks {
            let _ = t.await;
        }
    }
}

/// Publish [`EventKind::ApprovalRequested`] on `bus` with the given
/// channel routing, then await a matching
/// [`EventKind::ApprovalGranted`] / [`EventKind::ApprovalDenied`]
/// (filtered by `correlation_id`) until `timeout` elapses.
///
/// On timeout (or bus closure), synthesises an
/// `ApprovalDenied { reason: "timeout" }` so the audit trail records
/// the fail-closed decision and returns `false`. The caller is the
/// orchestrator — channel plugins on the bus are what actually drive
/// the human surface.
pub async fn request_approval_via_bus(
    bus: &EventBus,
    stream_id: &str,
    correlation_id: Uuid,
    channel: &str,
    description: &str,
    explanation: Option<&str>,
    timeout: Duration,
) -> bool {
    let approval_id = Uuid::new_v4().to_string();

    // Subscribe BEFORE publishing — otherwise a synchronous channel
    // could publish the decision before we attach.
    let mut rx = bus.subscribe_correlation(correlation_id).await;

    let request = Event::new(
        stream_id.to_string(),
        EventKind::ApprovalRequested {
            approval_id: approval_id.clone(),
            stream_id: stream_id.to_string(),
            description: description.to_string(),
            explanation: explanation.map(|s| s.to_string()),
            channel: channel.to_string(),
        },
        "orchestrator".into(),
    )
    .with_correlation(correlation_id);

    if let Err(e) = bus.publish(request).await {
        log::warn!("approval request publish failed: {e}");
        return false;
    }

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => match &ev.kind {
                EventKind::ApprovalGranted { approval_id: aid, .. } if aid == &approval_id => {
                    return true;
                }
                EventKind::ApprovalDenied { approval_id: aid, .. } if aid == &approval_id => {
                    return false;
                }
                _ => continue,
            },
            // bus subscription closed
            Ok(None) => break,
            // outer timeout fired
            Err(_) => break,
        }
    }

    // Timed out or bus closed. Synthesise a denial so observers see
    // *something* in the audit trail and the orchestrator's
    // OrchestratorResult line up with the published decision.
    let denied = Event::new(
        stream_id.to_string(),
        EventKind::ApprovalDenied {
            approval_id,
            stream_id: stream_id.to_string(),
            decided_by: "orchestrator".into(),
            reason: Some("timeout".into()),
        },
        "orchestrator".into(),
    )
    .with_correlation(correlation_id);
    if let Err(e) = bus.publish(denied).await {
        log::warn!("approval timeout publish failed: {e}");
    }
    false
}

/// Terminal-driven [`ApprovalChannel`] (ADR-0022). Subscribes to the
/// event bus, picks up [`EventKind::ApprovalRequested`] addressed to
/// this channel, prompts on stdin (serialised across requests so
/// concurrent agents don't garble the prompt), and publishes
/// [`EventKind::ApprovalGranted`] / [`EventKind::ApprovalDenied`] back
/// onto the bus.
///
/// Default channel when `approvals:` is absent from `project.yml` and
/// stdin is attached — preserves the zero-config UX of the previous
/// `TerminalApprovalHandler`.
pub struct TerminalApprovalChannel {
    name: String,
    /// Serialises blocking prompts so two concurrent requests don't
    /// interleave their output on the terminal.
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl TerminalApprovalChannel {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

#[async_trait]
impl ApprovalChannel for TerminalApprovalChannel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn start(&self, bus: Arc<EventBus>) -> Result<ChannelHandle, String> {
        let mut rx = bus.subscribe().await;
        let channel_name = self.name.clone();
        let lock = Arc::clone(&self.lock);

        let join = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let EventKind::ApprovalRequested {
                    approval_id,
                    stream_id,
                    description,
                    explanation,
                    channel,
                } = &ev.kind
                else {
                    continue;
                };
                if channel != &channel_name {
                    continue;
                }

                let description = description.clone();
                let explanation = explanation.clone();
                let approval_id = approval_id.clone();
                let stream_id_owned = stream_id.clone();
                let correlation_id = ev.correlation_id;
                let lock = Arc::clone(&lock);
                let bus = Arc::clone(&bus);
                let channel_name_owned = channel_name.clone();

                // Each request runs on its own task so a slow human
                // typing at the prompt can't pin the bus subscriber.
                tokio::spawn(async move {
                    let _guard = lock.lock().await;
                    let approved = tokio::task::spawn_blocking(move || {
                        terminal_prompt_blocking(&description, explanation.as_deref())
                            .unwrap_or(false)
                    })
                    .await
                    .unwrap_or(false);

                    let decision = if approved {
                        EventKind::ApprovalGranted {
                            approval_id,
                            stream_id: stream_id_owned.clone(),
                            decided_by: format!("terminal:{channel_name_owned}"),
                            reason: None,
                        }
                    } else {
                        EventKind::ApprovalDenied {
                            approval_id,
                            stream_id: stream_id_owned.clone(),
                            decided_by: format!("terminal:{channel_name_owned}"),
                            reason: None,
                        }
                    };
                    let mut decision_event =
                        Event::new(stream_id_owned, decision, "approval_channel".into());
                    if let Some(corr) = correlation_id {
                        decision_event = decision_event.with_correlation(corr);
                    }
                    if let Err(e) = bus.publish(decision_event).await {
                        log::warn!("terminal approval decision publish failed: {e}");
                    }
                });
            }
        });

        Ok(ChannelHandle::from_task(self.name.clone(), join))
    }
}

/// `type: terminal` builder for the YAML approval-channel registry.
///
/// Config shape:
/// ```yaml
/// approvals:
///   - type: terminal
///     name: local        # optional; defaults to "terminal"
/// ```
pub struct TerminalApprovalBuilder;

#[derive(Debug, Deserialize)]
struct TerminalChannelConfig {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
}

impl PluginBuilder<dyn ApprovalChannel> for TerminalApprovalBuilder {
    fn type_name(&self) -> &'static str {
        "terminal"
    }

    fn build(
        &self,
        name: String,
        entry: YamlValue,
    ) -> Result<Box<dyn ApprovalChannel>, Box<dyn std::error::Error + Send + Sync>> {
        // Validate the entry — terminal carries no required config beyond
        // `name`, but a typo (`prompt: yes` etc.) should fail loudly.
        let _: TerminalChannelConfig = serde_yml::from_value(entry)?;
        Ok(Box::new(TerminalApprovalChannel::new(name)))
    }
}

/// Build the registry of every approval-channel type known to the
/// runtime. Mirrors [`crate::connectors::build_core_connectors`].
///
/// Behind the `webhook` feature, also registers
/// [`crate::webhook::HttpApprovalBuilder`] for `type: http`.
/// Behind the `connectors` feature (which carries the reqwest+axum
/// stack the Telegram channel needs), also registers
/// [`crate::approval::TelegramApprovalBuilder`] for `type: telegram`.
pub fn build_core_approval_channels() -> PluginRegistry<dyn ApprovalChannel> {
    let mut r: PluginRegistry<dyn ApprovalChannel> = PluginRegistry::new();
    r.register(Box::new(TerminalApprovalBuilder));
    #[cfg(feature = "webhook")]
    r.register(Box::new(crate::webhook::HttpApprovalBuilder));
    #[cfg(feature = "connectors")]
    r.register(Box::new(TelegramApprovalBuilder));
    r
}

/// Resolve every entry in `project.approvals:` into a started
/// [`ChannelHandle`]. Mirrors [`crate::connectors::spawn_connectors`]:
/// build errors (unknown `type:`, duplicate `name:`) abort startup;
/// per-channel `start()` failures are reported and skipped.
///
/// Returns the live handles plus a `(name, error)` list for any channel
/// that failed to start. Callers are expected to drain the handles via
/// [`ChannelHandle::shutdown`] at process exit.
pub async fn spawn_approval_channels(
    entries: &[YamlValue],
    bus: Arc<EventBus>,
) -> Result<ApprovalChannelStartup, String> {
    if entries.is_empty() {
        return Ok(ApprovalChannelStartup::default());
    }

    let registry = build_core_approval_channels();
    let built = registry
        .build_all(entries.to_vec())
        .map_err(|e| format!("approvals: {e}"))?;

    let mut handles = Vec::new();
    let mut failures = Vec::new();
    for (name, channel) in built {
        match channel.start(Arc::clone(&bus)).await {
            Ok(handle) => handles.push(handle),
            Err(err) => {
                log::warn!("approval channel '{name}' failed to start: {err}");
                failures.push((name, err));
            }
        }
    }
    Ok(ApprovalChannelStartup { handles, failures })
}

/// Outcome of [`spawn_approval_channels`].
#[derive(Default)]
pub struct ApprovalChannelStartup {
    pub handles: Vec<ChannelHandle>,
    pub failures: Vec<(String, String)>,
}

/// Resolve which approval channel to route an [`EventKind::ApprovalRequested`]
/// through (ADR-0022 §"Which channel handles which request"):
///
/// 1. Per-pipeline override (`pipeline.approval_channel:`)
/// 2. Project default (`default_approval_channel:`)
/// 3. `None` → orchestrator fail-closes (publishes `ApprovalDenied {
///    reason: "no_approval_channel" }`).
///
/// Per-action overrides are deferred (ADR-0022 §"Resolution order").
pub fn resolve_channel(
    pipeline_override: Option<&str>,
    project_default: Option<&str>,
) -> Option<String> {
    pipeline_override
        .map(str::to_string)
        .or_else(|| project_default.map(str::to_string))
}

/// Outcome of [`replay_unfulfilled_approvals`]. Caller usually only logs
/// these — the bus already carries the audit-trail events.
#[derive(Default, Debug)]
pub struct ApprovalReplayReport {
    /// Approvals whose age exceeded `timeout`; sealed with a synthesised
    /// `ApprovalDenied { reason: "restart_timeout" }`.
    pub expired: Vec<String>,
    /// Approvals still within their timeout window; redelivered on the
    /// bus so live channel listeners can re-prompt.
    pub redelivered: Vec<String>,
}

/// Walk the event store and reconcile any `ApprovalRequested` whose
/// matching `ApprovalGranted` / `ApprovalDenied` is missing — the
/// "startup replay" path of ADR-0022.
///
/// On a clean shutdown the orchestrator's await loop synthesises an
/// `ApprovalDenied { reason: "timeout" }` after the deadline, so the
/// store self-heals. After a hard crash the await loop is gone but the
/// `ApprovalRequested` row persists; without this replay, the audit
/// trail leaves the request open forever and live channel listeners
/// (which subscribe on the *current* bus) never see it.
///
/// For each unfulfilled request:
/// - If `Utc::now() - timestamp >= timeout` → publish a synthetic
///   `ApprovalDenied { reason: "restart_timeout" }` so the audit trail
///   closes.
/// - Otherwise → [`EventBus::redeliver`] the original event so freshly
///   started channels can re-prompt. Redeliver does *not* re-append, so
///   the store is unchanged.
///
/// `timeout` should match the runtime's configured approval timeout
/// (defaults to [`DEFAULT_APPROVAL_TIMEOUT`]). The store doesn't capture
/// the per-request timeout, so we apply a single project-wide bound.
///
/// Channels must already be subscribed before calling this — otherwise
/// the redelivered events are dropped on the floor. Call after
/// [`spawn_approval_channels`] returns.
pub async fn replay_unfulfilled_approvals(
    bus: Arc<EventBus>,
    timeout: Duration,
) -> Result<ApprovalReplayReport, String> {
    use chrono::Utc;
    use std::collections::HashMap;

    let store = Arc::clone(bus.store());

    // Pass 1: scan the entire store, collecting outstanding
    // ApprovalRequested events and the set of decided approval_ids.
    // We need *both* the original event (for redeliver) and its
    // timestamp/correlation_id (for the synthetic denial) — keep the
    // full Event handy.
    let mut pending: HashMap<String, Event> = HashMap::new();
    let mut decided: std::collections::HashSet<String> = std::collections::HashSet::new();

    let batch = 1024usize;
    let mut from = 0u64;
    loop {
        let tailed = store
            .tail(from, batch)
            .await
            .map_err(|e| format!("approval replay: tail: {e}"))?;
        if tailed.is_empty() {
            break;
        }
        for t in &tailed {
            match &t.event.kind {
                EventKind::ApprovalRequested { approval_id, .. } => {
                    pending.insert(approval_id.clone(), t.event.clone());
                }
                EventKind::ApprovalGranted { approval_id, .. }
                | EventKind::ApprovalDenied { approval_id, .. } => {
                    decided.insert(approval_id.clone());
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

    pending.retain(|id, _| !decided.contains(id));
    if pending.is_empty() {
        return Ok(ApprovalReplayReport::default());
    }

    let now = Utc::now();
    let timeout_chrono = chrono::Duration::from_std(timeout)
        .unwrap_or_else(|_| chrono::Duration::seconds(300));

    let mut report = ApprovalReplayReport::default();
    for (approval_id, ev) in pending {
        let age = now.signed_duration_since(ev.timestamp);
        if age >= timeout_chrono {
            // Expired across the restart — seal the audit trail.
            let stream_id = match &ev.kind {
                EventKind::ApprovalRequested { stream_id, .. } => stream_id.clone(),
                _ => unreachable!("pending only holds ApprovalRequested"),
            };
            let denied = Event::new(
                stream_id.clone(),
                EventKind::ApprovalDenied {
                    approval_id: approval_id.clone(),
                    stream_id,
                    decided_by: "orchestrator".into(),
                    reason: Some("restart_timeout".into()),
                },
                "orchestrator".into(),
            );
            let denied = if let Some(corr) = ev.correlation_id {
                denied.with_correlation(corr)
            } else {
                denied
            };
            if let Err(e) = bus.publish(denied).await {
                log::warn!("approval replay: failed to publish restart_timeout for {approval_id}: {e}");
                continue;
            }
            report.expired.push(approval_id);
        } else {
            // Still in-flight — let live channels see the request again.
            bus.redeliver(Arc::new(ev)).await;
            report.redelivered.push(approval_id);
        }
    }

    Ok(report)
}

/// Telegram-driven [`ApprovalChannel`] (ADR-0022). Builds on the same
/// reqwest + axum stack that backs `http_post` / `http_inbound` —
/// declarative composition rather than a new transport.
///
/// Lifecycle:
/// 1. On [`EventKind::ApprovalRequested`] for this channel, POST to
///    `https://api.telegram.org/bot<token>/sendMessage` with an
///    inline-keyboard offering ✅ approve / ❌ deny.
/// 2. The bot's webhook (configured separately via
///    `setWebhook`) hits this channel's bound axum endpoint with a
///    `callback_query` payload encoding the chosen verdict.
/// 3. The channel publishes [`EventKind::ApprovalGranted`] /
///    [`EventKind::ApprovalDenied`] back onto the bus.
///
/// `decided_by` records `telegram:<from.username>` so the audit trail
/// names the human who clicked.
#[cfg(feature = "connectors")]
pub struct TelegramApprovalChannel {
    config: TelegramChannelConfig,
}

#[cfg(feature = "connectors")]
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramChannelConfig {
    /// Channel name — matches `ApprovalRequested.channel`.
    #[serde(default)]
    pub name: Option<String>,
    /// Bot token (the `bot<TOKEN>` segment of every Telegram URL).
    /// Supports `${env.*}` template expansion at YAML load.
    pub bot_token: String,
    /// Telegram chat id the approval prompt is sent to (a user id, a
    /// group id, or a channel id). Must be numeric per the Telegram API.
    pub chat_id: i64,
    /// Local bind address for the callback webhook endpoint. Use
    /// `127.0.0.1:0` for an OS-picked port; in production use a public
    /// address Telegram can reach (or front it with ngrok / Cloudflare
    /// Tunnel and call `setWebhook` accordingly).
    pub bind: String,
    /// Optional callback path (defaults to `/telegram/callback`). Must
    /// match the URL passed to Telegram's `setWebhook` for this bot.
    #[serde(default = "default_telegram_callback_path")]
    pub callback_path: String,
    /// Optional `secret_token` enforced via the
    /// `X-Telegram-Bot-Api-Secret-Token` header (Telegram's recommended
    /// webhook auth). When `None`, the endpoint is open — only safe
    /// for local development.
    #[serde(default)]
    pub secret_token: Option<String>,
}

#[cfg(feature = "connectors")]
fn default_telegram_callback_path() -> String {
    "/telegram/callback".into()
}

#[cfg(feature = "connectors")]
impl TelegramApprovalChannel {
    pub fn new(config: TelegramChannelConfig) -> Self {
        Self { config }
    }

    fn channel_name(&self) -> String {
        self.config.name.clone().unwrap_or_else(|| "telegram".into())
    }
}

#[cfg(feature = "connectors")]
#[async_trait]
impl ApprovalChannel for TelegramApprovalChannel {
    fn name(&self) -> &str {
        // Borrow against the option only when present; we can't return
        // an owned String, so default-name channels report the literal
        // "telegram" as a backup.
        self.config.name.as_deref().unwrap_or("telegram")
    }

    async fn start(&self, bus: Arc<EventBus>) -> Result<ChannelHandle, String> {
        use std::net::SocketAddr;

        let bind: SocketAddr = self
            .config
            .bind
            .parse()
            .map_err(|e| format!("invalid bind '{}': {e}", self.config.bind))?;
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .map_err(|e| format!("failed to bind {bind}: {e}"))?;
        let actual_addr = listener
            .local_addr()
            .map_err(|e| format!("failed to read bound addr: {e}"))?;

        let channel_name = self.channel_name();
        log::info!(
            "telegram approval channel '{channel_name}' listening on {actual_addr}{}",
            self.config.callback_path
        );

        // Webhook task: receive callback_query payloads and publish decisions.
        let webhook_state = Arc::new(TelegramWebhookState {
            channel_name: channel_name.clone(),
            bus: Arc::clone(&bus),
            secret_token: self.config.secret_token.clone(),
        });
        let app = telegram_router(Arc::clone(&webhook_state), self.config.callback_path.clone());
        let server_join = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                log::error!("telegram approval webhook error: {e}");
            }
        });

        // Sender task: subscribe to the bus and POST sendMessage on
        // ApprovalRequested addressed to this channel.
        let sender_state = TelegramSenderState {
            channel_name: channel_name.clone(),
            bot_token: self.config.bot_token.clone(),
            chat_id: self.config.chat_id,
        };
        let bus_for_sender = Arc::clone(&bus);
        let sender_join = tokio::spawn(async move {
            telegram_sender_loop(sender_state, bus_for_sender).await;
        });

        Ok(ChannelHandle::from_tasks(
            channel_name,
            vec![server_join, sender_join],
        ))
    }
}

#[cfg(feature = "connectors")]
struct TelegramSenderState {
    channel_name: String,
    bot_token: String,
    chat_id: i64,
}

#[cfg(feature = "connectors")]
async fn telegram_sender_loop(state: TelegramSenderState, bus: Arc<EventBus>) {
    let client = reqwest::Client::new();
    let send_url = format!(
        "https://api.telegram.org/bot{}/sendMessage",
        state.bot_token
    );
    let mut rx = bus.subscribe().await;
    while let Some(ev) = rx.recv().await {
        let EventKind::ApprovalRequested {
            approval_id,
            description,
            explanation,
            channel,
            ..
        } = &ev.kind
        else {
            continue;
        };
        if channel != &state.channel_name {
            continue;
        }

        let mut text = format!("Approval required\n\n{description}");
        if let Some(reason) = explanation {
            text.push_str("\n\nReason: ");
            text.push_str(reason);
        }

        // callback_data layout: "<approval_id>:approve" | "<approval_id>:deny".
        // Telegram caps callback_data at 64 bytes; UUID v4 + verdict fits.
        let payload = serde_json::json!({
            "chat_id": state.chat_id,
            "text": text,
            "reply_markup": {
                "inline_keyboard": [[
                    {"text": "✅ Approve", "callback_data": format!("{approval_id}:approve")},
                    {"text": "❌ Deny",    "callback_data": format!("{approval_id}:deny")},
                ]]
            }
        });

        match client.post(&send_url).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                log::warn!("telegram sendMessage failed ({status}): {body}");
            }
            Err(e) => log::warn!("telegram sendMessage transport error: {e}"),
        }
    }
}

#[cfg(feature = "connectors")]
struct TelegramWebhookState {
    channel_name: String,
    bus: Arc<EventBus>,
    secret_token: Option<String>,
}

#[cfg(feature = "connectors")]
fn telegram_router(
    state: Arc<TelegramWebhookState>,
    callback_path: String,
) -> axum::Router {
    use axum::routing::post;
    axum::Router::new()
        .route(&callback_path, post(telegram_callback_handler))
        .with_state(state)
}

#[cfg(feature = "connectors")]
async fn telegram_callback_handler(
    axum::extract::State(state): axum::extract::State<Arc<TelegramWebhookState>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::http::StatusCode {
    if let Some(expected) = state.secret_token.as_deref() {
        let header = headers
            .get("x-telegram-bot-api-secret-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if header != expected {
            return axum::http::StatusCode::UNAUTHORIZED;
        }
    }

    let Some(callback) = body.get("callback_query") else {
        // Other update types (message, edited_message, …) are ignored.
        return axum::http::StatusCode::OK;
    };
    let Some(data) = callback.get("data").and_then(|v| v.as_str()) else {
        return axum::http::StatusCode::OK;
    };
    let Some((approval_id, verdict)) = data.split_once(':') else {
        return axum::http::StatusCode::OK;
    };

    let approved = match verdict {
        "approve" => true,
        "deny" => false,
        _ => return axum::http::StatusCode::OK,
    };

    let decided_by = callback
        .get("from")
        .and_then(|f| f.get("username").and_then(|v| v.as_str()))
        .map(|u| format!("telegram:{u}"))
        .unwrap_or_else(|| "telegram".into());

    // Find the originating ApprovalRequested so we can mirror its
    // stream_id + correlation_id (same shape as the HTTP channel).
    let store = state.bus.store();
    let mut from = 0u64;
    let batch = 1024usize;
    let mut originating: Option<(String, Option<uuid::Uuid>)> = None;
    'walk: loop {
        let tailed = match store.tail(from, batch).await {
            Ok(t) => t,
            Err(e) => {
                log::error!("telegram callback store tail failed: {e}");
                return axum::http::StatusCode::INTERNAL_SERVER_ERROR;
            }
        };
        if tailed.is_empty() {
            break;
        }
        for t in &tailed {
            if let EventKind::ApprovalRequested {
                approval_id: aid,
                stream_id,
                channel,
                ..
            } = &t.event.kind
            {
                if aid == approval_id && channel == &state.channel_name {
                    originating = Some((stream_id.clone(), t.event.correlation_id));
                    break 'walk;
                }
            }
        }
        if tailed.len() < batch {
            break;
        }
        from = tailed.last().map(|t| t.global_seq).unwrap_or(from);
    }

    let Some((stream_id, correlation)) = originating else {
        return axum::http::StatusCode::NOT_FOUND;
    };

    let kind = if approved {
        EventKind::ApprovalGranted {
            approval_id: approval_id.to_string(),
            stream_id: stream_id.clone(),
            decided_by,
            reason: None,
        }
    } else {
        EventKind::ApprovalDenied {
            approval_id: approval_id.to_string(),
            stream_id: stream_id.clone(),
            decided_by,
            reason: None,
        }
    };
    let mut event = Event::new(stream_id, kind, "approval_channel".into());
    if let Some(corr) = correlation {
        event = event.with_correlation(corr);
    }
    if let Err(e) = state.bus.publish(event).await {
        log::error!("telegram callback decision publish failed: {e}");
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR;
    }

    axum::http::StatusCode::OK
}

/// `type: telegram` builder for the YAML approval-channel registry.
#[cfg(feature = "connectors")]
pub struct TelegramApprovalBuilder;

#[cfg(feature = "connectors")]
impl PluginBuilder<dyn ApprovalChannel> for TelegramApprovalBuilder {
    fn type_name(&self) -> &'static str {
        "telegram"
    }

    fn build(
        &self,
        name: String,
        entry: YamlValue,
    ) -> Result<Box<dyn ApprovalChannel>, Box<dyn std::error::Error + Send + Sync>> {
        let mut cfg: TelegramChannelConfig = serde_yml::from_value(entry)?;
        // The registry resolved `name` (defaulting to `type:` when absent);
        // mirror it onto the typed config so `name()` returns the right value.
        cfg.name = Some(name);
        Ok(Box::new(TelegramApprovalChannel::new(cfg)))
    }
}

// Surface the registry error type at module level for callers that
// need to handle it explicitly (e.g. CLI wrappers).
#[allow(dead_code)]
type ApprovalRegistryError = PluginBuildError;

#[cfg(all(test, feature = "connectors"))]
mod telegram_tests {
    use super::*;
    use crate::events::store::SqliteEventStore;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::time::Duration;
    use tempfile::TempDir;
    use tower::ServiceExt;

    /// The full flow: simulate a Telegram callback_query for an
    /// approve verdict and confirm the channel publishes
    /// `ApprovalGranted` keyed by the original `approval_id`,
    /// preserving correlation_id, with `decided_by` reflecting the
    /// Telegram username.
    #[tokio::test]
    async fn callback_handler_publishes_granted_with_correlation() {
        let dir = TempDir::new().unwrap();
        let store =
            Arc::new(SqliteEventStore::new(&dir.path().join("tg_approval.db")).unwrap());
        let bus = Arc::new(EventBus::new(store));

        // Original approval request on the bus + store.
        let correlation = Uuid::new_v4();
        let request = Event::new(
            "stream-x".into(),
            EventKind::ApprovalRequested {
                approval_id: "req-7".into(),
                stream_id: "stream-x".into(),
                description: "rm -rf /tmp".into(),
                explanation: None,
                channel: "ops_tg".into(),
            },
            "orchestrator".into(),
        )
        .with_correlation(correlation);
        bus.publish(request).await.unwrap();

        let mut rx = bus.subscribe_correlation(correlation).await;

        let state = Arc::new(TelegramWebhookState {
            channel_name: "ops_tg".into(),
            bus: Arc::clone(&bus),
            secret_token: None,
        });
        let app = telegram_router(state, "/cb".into());

        let body = serde_json::json!({
            "update_id": 1,
            "callback_query": {
                "id": "cb-1",
                "from": {"id": 1, "username": "alice"},
                "data": "req-7:approve",
            }
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cb")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let ev = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("decision event timeout")
            .expect("bus closed");
        match &ev.kind {
            EventKind::ApprovalGranted {
                approval_id,
                decided_by,
                ..
            } => {
                assert_eq!(approval_id, "req-7");
                assert_eq!(decided_by, "telegram:alice");
            }
            other => panic!("expected ApprovalGranted, got {other:?}"),
        }
        assert_eq!(ev.correlation_id, Some(correlation));
    }

    /// `secret_token` mismatch returns 401 and publishes nothing.
    #[tokio::test]
    async fn callback_handler_rejects_bad_secret_token() {
        let dir = TempDir::new().unwrap();
        let store =
            Arc::new(SqliteEventStore::new(&dir.path().join("tg_secret.db")).unwrap());
        let bus = Arc::new(EventBus::new(store));

        let state = Arc::new(TelegramWebhookState {
            channel_name: "ops_tg".into(),
            bus,
            secret_token: Some("expected".into()),
        });
        let app = telegram_router(state, "/cb".into());

        let body = serde_json::json!({"callback_query": {"data": "x:approve"}});
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cb")
                    .header("content-type", "application/json")
                    .header("x-telegram-bot-api-secret-token", "wrong")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}

#[cfg(test)]
mod resolver_tests {
    use super::*;

    #[test]
    fn pipeline_override_wins_over_project_default() {
        let resolved = resolve_channel(Some("ops_slack"), Some("local"));
        assert_eq!(resolved.as_deref(), Some("ops_slack"));
    }

    #[test]
    fn project_default_used_when_pipeline_unset() {
        let resolved = resolve_channel(None, Some("local"));
        assert_eq!(resolved.as_deref(), Some("local"));
    }

    #[test]
    fn fail_closed_when_neither_set() {
        assert!(resolve_channel(None, None).is_none());
    }

    #[test]
    fn terminal_builder_dispatches_via_registry() {
        let registry = build_core_approval_channels();
        let entry: YamlValue = serde_yml::from_str("type: terminal\nname: local").unwrap();
        let (name, channel) = registry.build_one(entry).unwrap();
        assert_eq!(name, "local");
        assert_eq!(channel.name(), "local");
    }

    #[cfg(feature = "webhook")]
    #[test]
    fn http_builder_dispatches_via_registry() {
        let registry = build_core_approval_channels();
        let entry: YamlValue = serde_yml::from_str(
            "type: http\nname: ops_api\nbind: \"127.0.0.1:0\"\nbearer_token: t",
        )
        .unwrap();
        let (name, channel) = registry.build_one(entry).unwrap();
        assert_eq!(name, "ops_api");
        assert_eq!(channel.name(), "ops_api");
    }

    #[cfg(feature = "connectors")]
    #[test]
    fn telegram_builder_dispatches_via_registry() {
        let registry = build_core_approval_channels();
        let entry: YamlValue = serde_yml::from_str(
            "type: telegram\nname: ops_tg\nbot_token: tok\nchat_id: 42\nbind: 127.0.0.1:0",
        )
        .unwrap();
        let (name, channel) = registry.build_one(entry).unwrap();
        assert_eq!(name, "ops_tg");
        assert_eq!(channel.name(), "ops_tg");
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use crate::events::store::SqliteEventStore;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, Arc<EventBus>) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(SqliteEventStore::new(&dir.path().join("replay.db")).unwrap());
        let bus = Arc::new(EventBus::new(store));
        (dir, bus)
    }

    /// Approval older than the timeout window gets sealed with
    /// `restart_timeout` so the audit trail closes.
    #[tokio::test]
    async fn expired_request_sealed_with_restart_timeout() {
        let (_dir, bus) = setup().await;

        // Forge an ApprovalRequested with a stale timestamp.
        let mut stale = Event::new(
            "stream-A".into(),
            EventKind::ApprovalRequested {
                approval_id: "old-1".into(),
                stream_id: "stream-A".into(),
                description: "rm -rf /".into(),
                explanation: None,
                channel: "terminal".into(),
            },
            "orchestrator".into(),
        );
        stale.timestamp = chrono::Utc::now() - chrono::Duration::seconds(3600);
        bus.store().append(&mut stale.clone()).await.unwrap();

        let report = replay_unfulfilled_approvals(Arc::clone(&bus), Duration::from_secs(60))
            .await
            .unwrap();

        assert_eq!(report.expired, vec!["old-1".to_string()]);
        assert!(report.redelivered.is_empty());

        // ApprovalDenied{restart_timeout} now lives in the store.
        let mut from = 0u64;
        let mut found = false;
        loop {
            let batch = bus.store().tail(from, 256).await.unwrap();
            if batch.is_empty() {
                break;
            }
            for t in &batch {
                if let EventKind::ApprovalDenied { approval_id, reason, .. } = &t.event.kind {
                    if approval_id == "old-1" && reason.as_deref() == Some("restart_timeout") {
                        found = true;
                    }
                }
            }
            from = batch.last().unwrap().global_seq;
            if batch.len() < 256 {
                break;
            }
        }
        assert!(found, "expected ApprovalDenied{{restart_timeout}} in store");
    }

    /// In-flight approval (still within timeout window) is redelivered to
    /// live subscribers; the store gains no new events.
    ///
    /// Append straight to the store instead of [`EventBus::publish`] —
    /// publish records the id in the bus's local-dedup ring, which is
    /// exactly what suppresses double-delivery from the tail watcher.
    /// We're modelling a *fresh* process where that ring is empty.
    #[tokio::test]
    async fn in_flight_request_redelivered() {
        let (_dir, bus) = setup().await;

        let mut req = Event::new(
            "stream-B".into(),
            EventKind::ApprovalRequested {
                approval_id: "fresh-1".into(),
                stream_id: "stream-B".into(),
                description: "hello".into(),
                explanation: None,
                channel: "terminal".into(),
            },
            "orchestrator".into(),
        );
        bus.store().append(&mut req).await.unwrap();
        let pre_seq = bus.store().current_global_seq().await.unwrap();

        let mut rx = bus.subscribe().await;

        let report = replay_unfulfilled_approvals(Arc::clone(&bus), Duration::from_secs(3600))
            .await
            .unwrap();

        assert_eq!(report.redelivered, vec!["fresh-1".to_string()]);
        assert!(report.expired.is_empty());

        // Subscriber sees the redelivered event.
        let ev = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("redelivered event timeout")
            .expect("bus closed");
        match &ev.kind {
            EventKind::ApprovalRequested { approval_id, .. } => {
                assert_eq!(approval_id, "fresh-1");
            }
            other => panic!("expected ApprovalRequested, got {other:?}"),
        }

        // Store unchanged — redeliver does not append.
        assert_eq!(bus.store().current_global_seq().await.unwrap(), pre_seq);
    }

    /// Already-decided approvals are ignored — replay touches nothing.
    #[tokio::test]
    async fn decided_request_ignored() {
        let (_dir, bus) = setup().await;

        let req = Event::new(
            "stream-C".into(),
            EventKind::ApprovalRequested {
                approval_id: "done-1".into(),
                stream_id: "stream-C".into(),
                description: "x".into(),
                explanation: None,
                channel: "terminal".into(),
            },
            "orchestrator".into(),
        );
        bus.publish(req).await.unwrap();

        let granted = Event::new(
            "stream-C".into(),
            EventKind::ApprovalGranted {
                approval_id: "done-1".into(),
                stream_id: "stream-C".into(),
                decided_by: "alice".into(),
                reason: None,
            },
            "channel".into(),
        );
        bus.publish(granted).await.unwrap();

        let report = replay_unfulfilled_approvals(Arc::clone(&bus), Duration::from_secs(3600))
            .await
            .unwrap();

        assert!(report.expired.is_empty());
        assert!(report.redelivered.is_empty());
    }
}

fn terminal_prompt_blocking(
    description: &str,
    explanation: Option<&str>,
) -> Result<bool, String> {
    use std::io::{self, BufRead, Write};

    {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        writeln!(out).ok();
        writeln!(out, "--- approval required ----------------------------").ok();
        writeln!(out, "  {description}").ok();
        if let Some(reason) = explanation {
            writeln!(out, "  reason: {reason}").ok();
        }
        write!(out, "  approve? [y/N]: ").ok();
        out.flush().ok();
    }

    let stdin = io::stdin();
    let mut line = String::new();
    match stdin.lock().read_line(&mut line) {
        Ok(0) => Ok(false), // EOF — deny
        Ok(_) => {
            let answer = line.trim().to_ascii_lowercase();
            Ok(matches!(answer.as_str(), "y" | "yes"))
        }
        Err(_) => Ok(false), // IO error — deny
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::store::SqliteEventStore;
    use tempfile::TempDir;

    /// `request_approval_via_bus` returns `true` when a Granted event
    /// matching the approval_id arrives, even when other unrelated
    /// events show up in between.
    #[tokio::test]
    async fn request_via_bus_returns_true_on_grant() {
        let dir = TempDir::new().unwrap();
        let store =
            Arc::new(SqliteEventStore::new(&dir.path().join("approval_bus.db")).unwrap());
        let bus = Arc::new(EventBus::new(store));

        let stream_id = "stream-1".to_string();
        let correlation_id = Uuid::new_v4();
        let bus_for_listener = Arc::clone(&bus);
        let listener = tokio::spawn(async move {
            let mut rx = bus_for_listener.subscribe().await;
            while let Some(ev) = rx.recv().await {
                if let EventKind::ApprovalRequested {
                    approval_id,
                    stream_id,
                    ..
                } = &ev.kind
                {
                    let mut grant = Event::new(
                        stream_id.clone(),
                        EventKind::ApprovalGranted {
                            approval_id: approval_id.clone(),
                            stream_id: stream_id.clone(),
                            decided_by: "test".into(),
                            reason: None,
                        },
                        "approval_channel".into(),
                    );
                    if let Some(corr) = ev.correlation_id {
                        grant = grant.with_correlation(corr);
                    }
                    bus_for_listener.publish(grant).await.unwrap();
                    break;
                }
            }
        });

        let approved = request_approval_via_bus(
            bus.as_ref(),
            &stream_id,
            correlation_id,
            "test-channel",
            "do the thing",
            None,
            Duration::from_secs(2),
        )
        .await;
        assert!(approved);
        listener.await.unwrap();
    }
}
