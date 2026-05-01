use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use crate::events::bus::EventBus;
use crate::events::{Event, EventKind};

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
    async fn start(self: Arc<Self>, bus: Arc<EventBus>) -> Result<ChannelHandle, String>;
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

    async fn start(self: Arc<Self>, bus: Arc<EventBus>) -> Result<ChannelHandle, String> {
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
