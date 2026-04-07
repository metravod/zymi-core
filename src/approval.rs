use std::sync::Arc;

use async_trait::async_trait;

#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn request_approval(
        &self,
        tool_description: &str,
        explanation: Option<&str>,
    ) -> Result<bool, String>;
}

/// A shared slot for the current approval handler.
/// Connectors set it before calling agent.process_stream() and clear it after.
/// Sub-agents read from this slot to get shell approval capability.
pub type SharedApprovalHandler = Arc<tokio::sync::RwLock<Option<Arc<dyn ApprovalHandler>>>>;

pub fn new_shared_approval_handler() -> SharedApprovalHandler {
    Arc::new(tokio::sync::RwLock::new(None))
}

/// RAII guard that sets a handler into the shared slot on creation
/// and clears it on drop — even if the owner panics.
pub struct ApprovalSlotGuard {
    slot: SharedApprovalHandler,
}

impl ApprovalSlotGuard {
    pub async fn set(slot: SharedApprovalHandler, handler: Arc<dyn ApprovalHandler>) -> Self {
        {
            let mut s = slot.write().await;
            *s = Some(handler);
        }
        Self { slot }
    }
}

impl Drop for ApprovalSlotGuard {
    fn drop(&mut self) {
        if let Ok(mut s) = self.slot.try_write() {
            *s = None;
        } else {
            let slot = self.slot.clone();
            tokio::spawn(async move {
                let mut s = slot.write().await;
                *s = None;
            });
        }
    }
}

/// Wraps an inner `ApprovalHandler`, prepending context (e.g. sub-agent name)
/// to the tool description shown to the user.
pub struct ContextualApprovalHandler {
    inner: Arc<dyn ApprovalHandler>,
    context: String,
}

impl ContextualApprovalHandler {
    pub fn new(inner: Arc<dyn ApprovalHandler>, context: String) -> Self {
        Self { inner, context }
    }
}

#[async_trait]
impl ApprovalHandler for ContextualApprovalHandler {
    async fn request_approval(
        &self,
        tool_description: &str,
        explanation: Option<&str>,
    ) -> Result<bool, String> {
        let prefixed = format!("[{}] {}", self.context, tool_description);
        self.inner.request_approval(&prefixed, explanation).await
    }
}

/// Terminal-based [`ApprovalHandler`] for interactive operators.
///
/// Prompts on stdout / reads y/N from stdin. Used by `zymi run`,
/// `zymi serve`, and the Python `Runtime` binding when
/// `approval="terminal"` (the default). Lives at the root of the
/// approval module so it is available to any caller that enables the
/// `runtime` feature, not just the `cli` feature.
///
/// The blocking IO runs inside [`tokio::task::spawn_blocking`] so the
/// runtime is not stalled, and a [`tokio::sync::Mutex`] serialises
/// prompts across parallel agent steps so concurrent loops do not
/// interleave on the terminal.
///
/// Default policy on EOF / non-tty / IO error: **deny**. If we cannot
/// ask the human, we do not approve — that is the whole point of
/// having a handler in the first place.
pub struct TerminalApprovalHandler {
    /// Serialises prompts so parallel agent steps don't interleave
    /// their questions on the terminal.
    lock: tokio::sync::Mutex<()>,
}

impl TerminalApprovalHandler {
    pub fn new() -> Self {
        Self {
            lock: tokio::sync::Mutex::new(()),
        }
    }
}

impl Default for TerminalApprovalHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ApprovalHandler for TerminalApprovalHandler {
    async fn request_approval(
        &self,
        tool_description: &str,
        explanation: Option<&str>,
    ) -> Result<bool, String> {
        let _guard = self.lock.lock().await;

        let description = tool_description.to_string();
        let explanation = explanation.map(|s| s.to_string());

        tokio::task::spawn_blocking(move || {
            terminal_prompt_blocking(&description, explanation.as_deref())
        })
        .await
        .map_err(|e| format!("approval prompt task failed: {e}"))?
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
    use std::sync::atomic::{AtomicBool, Ordering};

    struct MockApprovalHandler {
        approved: bool,
        last_description: tokio::sync::Mutex<String>,
    }

    impl MockApprovalHandler {
        fn new(approved: bool) -> Self {
            Self {
                approved,
                last_description: tokio::sync::Mutex::new(String::new()),
            }
        }
    }

    #[async_trait]
    impl ApprovalHandler for MockApprovalHandler {
        async fn request_approval(
            &self,
            tool_description: &str,
            _explanation: Option<&str>,
        ) -> Result<bool, String> {
            *self.last_description.lock().await = tool_description.to_string();
            Ok(self.approved)
        }
    }

    #[tokio::test]
    async fn guard_sets_and_clears_slot() {
        let slot = new_shared_approval_handler();
        assert!(slot.read().await.is_none());

        let handler: Arc<dyn ApprovalHandler> = Arc::new(MockApprovalHandler::new(true));
        {
            let _guard = ApprovalSlotGuard::set(slot.clone(), handler).await;
            assert!(slot.read().await.is_some());
        }
        // guard dropped — slot should be cleared
        assert!(slot.read().await.is_none());
    }

    #[tokio::test]
    async fn guard_clears_on_panic() {
        let slot = new_shared_approval_handler();
        let handler: Arc<dyn ApprovalHandler> = Arc::new(MockApprovalHandler::new(true));
        let cleared = Arc::new(AtomicBool::new(false));

        let slot_clone = slot.clone();
        let cleared_clone = cleared.clone();
        let result = std::panic::AssertUnwindSafe(async {
            let _guard = ApprovalSlotGuard::set(slot_clone, handler).await;
            panic!("test panic");
        });

        let _ = tokio::task::spawn(async move {
            let _ = futures::FutureExt::catch_unwind(result).await;
            cleared_clone.store(true, Ordering::SeqCst);
        })
        .await;

        assert!(cleared.load(Ordering::SeqCst));
        // After panic + drop, slot should be cleared
        assert!(slot.read().await.is_none());
    }

    #[tokio::test]
    async fn contextual_handler_prepends_context() {
        let inner = Arc::new(MockApprovalHandler::new(true));
        let handler = ContextualApprovalHandler::new(inner.clone(), "my-agent".to_string());

        let result = handler
            .request_approval("run shell command", None)
            .await
            .unwrap();
        assert!(result);
        assert_eq!(
            *inner.last_description.lock().await,
            "[my-agent] run shell command"
        );
    }
}
