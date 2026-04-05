use std::sync::Arc;

use uuid::Uuid;

use crate::approval::ApprovalHandler;
use crate::events::bus::EventBus;
use crate::events::{Event, EventKind};

use super::contracts::ContractEngine;
use super::{Intention, IntentionVerdict};

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
    /// 4. If RequiresHumanApproval: request approval, emit ApprovalDecided
    /// 5. Return result
    pub async fn process_intention(
        &self,
        intention: &Intention,
        stream_id: &str,
        correlation_id: Uuid,
        approval_handler: Option<&dyn ApprovalHandler>,
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
                let approval_id = Uuid::new_v4().to_string();

                self.emit(
                    stream_id,
                    correlation_id,
                    EventKind::ApprovalRequested {
                        description: reason.clone(),
                        approval_id: approval_id.clone(),
                    },
                )
                .await;

                let approved = match approval_handler {
                    Some(handler) => handler
                        .request_approval(&reason, None)
                        .await
                        .unwrap_or(false),
                    None => {
                        self.emit(
                            stream_id,
                            correlation_id,
                            EventKind::ApprovalDecided {
                                approval_id,
                                approved: false,
                            },
                        )
                        .await;
                        return OrchestratorResult::NoApprovalHandler;
                    }
                };

                self.emit(
                    stream_id,
                    correlation_id,
                    EventKind::ApprovalDecided {
                        approval_id,
                        approved,
                    },
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
                None,
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
                None,
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
                None,
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
        assert_eq!(e4.kind_tag(), "approval_decided");
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
                None,
            )
            .await;

        assert!(matches!(result, OrchestratorResult::Denied { .. }));
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
                None,
            )
            .await;

        assert!(matches!(result, OrchestratorResult::Approved));
    }
}
