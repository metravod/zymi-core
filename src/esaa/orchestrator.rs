use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::approval::request_approval_via_bus;
use crate::events::bus::EventBus;
use crate::events::{Event, EventKind};

use super::contracts::ContractEngine;
use super::{Intention, IntentionVerdict};

/// Per-request configuration for human approval routing (ADR-0022).
///
/// Constructed by callers (`run_pipeline.rs`) from runtime config and
/// passed through to [`Orchestrator::process_intention`]. When
/// `channel` is `None`, a `RequiresHumanApproval` verdict resolves to
/// [`OrchestratorResult::NoApprovalHandler`] (fail-closed).
pub struct ApprovalContext<'a> {
    /// Channel name to route this approval to. The orchestrator
    /// publishes `ApprovalRequested { channel }` and awaits a matching
    /// `ApprovalGranted` / `ApprovalDenied` from a channel plugin.
    pub channel: Option<&'a str>,
    /// Per-request timeout for the bus path. Used as the default
    /// "denied on timeout" budget.
    pub timeout: Duration,
}

impl<'a> ApprovalContext<'a> {
    pub fn none() -> Self {
        Self {
            channel: None,
            timeout: crate::approval::DEFAULT_APPROVAL_TIMEOUT,
        }
    }
}

/// Deterministic orchestrator for ESAA. Validates intentions through boundary
/// contracts, handles human approval when required, emits events at each step,
/// and returns the verdict.
///
/// The orchestrator does NOT execute the side effect — that remains the caller's
/// responsibility. This separation ensures the orchestrator is pure validation +
/// event emission, making it testable and replayable.
pub struct Orchestrator {
    contracts: Arc<ContractEngine>,
    bus: Arc<EventBus>,
}

/// Result of processing an intention through the orchestrator.
#[derive(Debug)]
pub enum OrchestratorResult {
    /// Intention approved (by contract or by human). Caller should execute.
    Approved,
    /// Intention denied by contract. Caller must not execute.
    Denied { reason: String },
    /// Human rejected the intention. Caller must not execute.
    HumanRejected,
    /// Approval was required but no handler was available. Caller must not execute.
    NoApprovalHandler,
}

impl Orchestrator {
    pub fn new(contracts: Arc<ContractEngine>, bus: Arc<EventBus>) -> Self {
        Self { contracts, bus }
    }

    /// Process an intention through the full ESAA pipeline:
    /// 1. Emit IntentionEmitted event
    /// 2. Evaluate against boundary contracts
    /// 3. Emit IntentionEvaluated event
    /// 4. If RequiresHumanApproval: route through the configured
    ///    [`ApprovalContext`] — either bus channel (publish-and-wait) or
    ///    the legacy in-process handler — and emit the matching
    ///    Approval{Requested, Granted, Denied} events.
    /// 5. Return result
    pub async fn process_intention(
        &self,
        intention: &Intention,
        stream_id: &str,
        correlation_id: Uuid,
        approval: ApprovalContext<'_>,
    ) -> OrchestratorResult {
        // 1. Emit IntentionEmitted
        let intention_data = serde_json::to_string(intention).unwrap_or_default();
        self.emit(
            stream_id,
            correlation_id,
            EventKind::IntentionEmitted {
                intention_tag: intention.tag().to_string(),
                intention_data,
            },
        )
        .await;

        // 2. Evaluate contracts
        let verdict = self.contracts.evaluate(intention);

        // 3. Emit IntentionEvaluated
        let verdict_str = match &verdict {
            IntentionVerdict::Approved => "approved".to_string(),
            IntentionVerdict::RequiresHumanApproval { reason } => {
                format!("requires_approval: {reason}")
            }
            IntentionVerdict::Denied { reason } => format!("denied: {reason}"),
        };
        self.emit(
            stream_id,
            correlation_id,
            EventKind::IntentionEvaluated {
                intention_tag: intention.tag().to_string(),
                verdict: verdict_str,
            },
        )
        .await;

        // 4. Handle verdict
        match verdict {
            IntentionVerdict::Approved => OrchestratorResult::Approved,
            IntentionVerdict::Denied { reason } => OrchestratorResult::Denied { reason },
            IntentionVerdict::RequiresHumanApproval { reason } => {
                // ADR-0022: orchestrator publishes ApprovalRequested
                // with the configured channel and awaits a matching
                // ApprovalGranted / ApprovalDenied from a channel
                // plugin. The bus helper owns publish + await + synth
                // ApprovalDenied{timeout} on deadline; this branch
                // emits no approval events directly.
                let Some(channel) = approval.channel else {
                    // Fail-closed: no channel configured. Record an
                    // audit-trail Requested + Denied so observers see
                    // the failure mode rather than a silent skip.
                    let approval_id = Uuid::new_v4().to_string();
                    self.emit(
                        stream_id,
                        correlation_id,
                        EventKind::ApprovalRequested {
                            approval_id: approval_id.clone(),
                            stream_id: stream_id.to_string(),
                            description: reason.clone(),
                            explanation: None,
                            channel: "<none>".into(),
                        },
                    )
                    .await;
                    self.emit(
                        stream_id,
                        correlation_id,
                        EventKind::ApprovalDenied {
                            approval_id,
                            stream_id: stream_id.to_string(),
                            decided_by: "orchestrator".into(),
                            reason: Some("no_approval_channel".into()),
                        },
                    )
                    .await;
                    return OrchestratorResult::NoApprovalHandler;
                };

                let approved = request_approval_via_bus(
                    self.bus.as_ref(),
                    stream_id,
                    correlation_id,
                    channel,
                    &reason,
                    None,
                    approval.timeout,
                )
                .await;

                if approved {
                    OrchestratorResult::Approved
                } else {
                    OrchestratorResult::HumanRejected
                }
            }
        }
    }

    async fn emit(&self, stream_id: &str, correlation_id: Uuid, kind: EventKind) {
        let event = Event::new(stream_id.to_string(), kind, "orchestrator".into())
            .with_correlation(correlation_id);

        if let Err(e) = self.bus.publish(event).await {
            log::warn!("Orchestrator failed to emit event: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::esaa::contracts::FileWriteContract;
    use crate::events::store::SqliteEventStore;
    use crate::policy::{PolicyConfig, PolicyEngine};
    use tempfile::TempDir;

    async fn setup() -> (TempDir, Arc<EventBus>, Arc<ContractEngine>) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_orch.db");
        let store = Arc::new(SqliteEventStore::new(&db_path).unwrap());
        let bus = Arc::new(EventBus::new(store));

        let policy = Arc::new(PolicyEngine::new(PolicyConfig {
            enabled: true,
            allow: vec!["ls*".into(), "cat*".into()],
            deny: vec!["rm*".into()],
            require_approval: vec!["sudo*".into()],
        }));

        let contracts = Arc::new(ContractEngine::new(
            policy,
            FileWriteContract {
                allowed_dirs: vec!["./memory/".into()],
                deny_patterns: vec!["*.env".into()],
            },
        ));

        (dir, bus, contracts)
    }

    #[tokio::test]
    async fn approved_intention_emits_events() {
        let (_dir, bus, contracts) = setup().await;
        let orch = Orchestrator::new(contracts, bus.clone());
        let mut rx = bus.subscribe().await;

        let result = orch
            .process_intention(
                &Intention::ExecuteShellCommand {
                    command: "ls".into(),
                    timeout_secs: None,
                },
                "test-stream",
                Uuid::new_v4(),
                ApprovalContext::none(),
            )
            .await;

        assert!(matches!(result, OrchestratorResult::Approved));

        let e1 = rx.recv().await.unwrap();
        assert_eq!(e1.kind_tag(), "intention_emitted");

        let e2 = rx.recv().await.unwrap();
        assert_eq!(e2.kind_tag(), "intention_evaluated");
    }

    #[tokio::test]
    async fn denied_intention_emits_events() {
        let (_dir, bus, contracts) = setup().await;
        let orch = Orchestrator::new(contracts, bus.clone());
        let mut rx = bus.subscribe().await;

        let result = orch
            .process_intention(
                &Intention::ExecuteShellCommand {
                    command: "rm -rf /".into(),
                    timeout_secs: None,
                },
                "test-stream",
                Uuid::new_v4(),
                ApprovalContext::none(),
            )
            .await;

        assert!(matches!(result, OrchestratorResult::Denied { .. }));

        let e1 = rx.recv().await.unwrap();
        assert_eq!(e1.kind_tag(), "intention_emitted");

        let e2 = rx.recv().await.unwrap();
        assert_eq!(e2.kind_tag(), "intention_evaluated");
    }

    #[tokio::test]
    async fn requires_approval_no_handler() {
        let (_dir, bus, contracts) = setup().await;
        let orch = Orchestrator::new(contracts, bus.clone());
        let mut rx = bus.subscribe().await;

        let result = orch
            .process_intention(
                &Intention::ExecuteShellCommand {
                    command: "sudo reboot".into(),
                    timeout_secs: None,
                },
                "test-stream",
                Uuid::new_v4(),
                ApprovalContext::none(),
            )
            .await;

        assert!(matches!(result, OrchestratorResult::NoApprovalHandler));

        let e1 = rx.recv().await.unwrap();
        assert_eq!(e1.kind_tag(), "intention_emitted");

        let e2 = rx.recv().await.unwrap();
        assert_eq!(e2.kind_tag(), "intention_evaluated");

        let e3 = rx.recv().await.unwrap();
        assert_eq!(e3.kind_tag(), "approval_requested");

        let e4 = rx.recv().await.unwrap();
        assert_eq!(e4.kind_tag(), "approval_denied");
    }

    #[tokio::test]
    async fn file_write_denied_by_pattern() {
        let (_dir, bus, contracts) = setup().await;
        let orch = Orchestrator::new(contracts, bus.clone());

        let result = orch
            .process_intention(
                &Intention::WriteFile {
                    path: "./.env".into(),
                    content: "SECRET=bad".into(),
                },
                "test-stream",
                Uuid::new_v4(),
                ApprovalContext::none(),
            )
            .await;

        assert!(matches!(result, OrchestratorResult::Denied { .. }));
    }

    /// Bus-path approval (ADR-0022): orchestrator publishes
    /// ApprovalRequested with a channel name; a fake channel listener
    /// publishes ApprovalGranted; orchestrator returns Approved.
    #[tokio::test]
    async fn requires_approval_bus_channel_grants() {
        let (_dir, bus, contracts) = setup().await;
        let orch = Orchestrator::new(contracts, bus.clone());

        // Fake channel: subscribe, watch for ApprovalRequested with our
        // channel name, publish a Granted decision keyed off the same
        // approval_id.
        let bus_for_channel = Arc::clone(&bus);
        let channel_task = tokio::spawn(async move {
            let mut rx = bus_for_channel.subscribe().await;
            while let Some(ev) = rx.recv().await {
                if let EventKind::ApprovalRequested {
                    approval_id,
                    stream_id,
                    channel,
                    ..
                } = &ev.kind
                {
                    if channel == "test-channel" {
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
                        bus_for_channel.publish(grant).await.unwrap();
                        break;
                    }
                }
            }
        });

        let result = orch
            .process_intention(
                &Intention::ExecuteShellCommand {
                    command: "sudo reboot".into(),
                    timeout_secs: None,
                },
                "test-stream",
                Uuid::new_v4(),
                ApprovalContext {
                    channel: Some("test-channel"),
                    timeout: Duration::from_secs(2),
                },
            )
            .await;

        assert!(matches!(result, OrchestratorResult::Approved));
        channel_task.await.unwrap();
    }

    /// Bus-path timeout: no channel listener responds within the
    /// timeout, helper synthesises an ApprovalDenied{reason: timeout}.
    #[tokio::test]
    async fn requires_approval_bus_channel_times_out() {
        let (_dir, bus, contracts) = setup().await;
        let orch = Orchestrator::new(contracts, bus.clone());
        let mut rx = bus.subscribe().await;

        let result = orch
            .process_intention(
                &Intention::ExecuteShellCommand {
                    command: "sudo reboot".into(),
                    timeout_secs: None,
                },
                "test-stream",
                Uuid::new_v4(),
                ApprovalContext {
                    channel: Some("nobody-home"),
                    timeout: Duration::from_millis(80),
                },
            )
            .await;

        assert!(matches!(result, OrchestratorResult::HumanRejected));

        // Walk the stream looking for the synthesised denial.
        let mut saw_requested = false;
        let mut saw_denied_with_timeout = false;
        for _ in 0..6 {
            let Ok(Some(ev)) =
                tokio::time::timeout(Duration::from_millis(200), rx.recv()).await
            else {
                break;
            };
            match &ev.kind {
                EventKind::ApprovalRequested { channel, .. } if channel == "nobody-home" => {
                    saw_requested = true;
                }
                EventKind::ApprovalDenied { reason, .. }
                    if reason.as_deref() == Some("timeout") =>
                {
                    saw_denied_with_timeout = true;
                }
                _ => {}
            }
            if saw_requested && saw_denied_with_timeout {
                break;
            }
        }
        assert!(saw_requested, "ApprovalRequested not observed");
        assert!(
            saw_denied_with_timeout,
            "synthesised ApprovalDenied{{timeout}} not observed"
        );
    }

    #[tokio::test]
    async fn file_write_in_allowed_dir() {
        let (_dir, bus, contracts) = setup().await;
        let orch = Orchestrator::new(contracts, bus.clone());

        let result = orch
            .process_intention(
                &Intention::WriteFile {
                    path: "./memory/notes.md".into(),
                    content: "hello".into(),
                },
                "test-stream",
                Uuid::new_v4(),
                ApprovalContext::none(),
            )
            .await;

        assert!(matches!(result, OrchestratorResult::Approved));
    }
}
