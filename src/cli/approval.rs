//! Terminal-based [`ApprovalHandler`] for `zymi run` and `zymi serve`.
//!
//! Prompts the operator on stdout / reads y/N from stdin. The actual blocking
//! IO runs inside [`tokio::task::spawn_blocking`] so the runtime is not stalled,
//! and a [`tokio::sync::Mutex`] serialises prompts across parallel agent steps
//! so concurrent loops do not interleave on the terminal.
//!
//! Default policy on EOF / non-tty / IO error: **deny**. If we cannot ask the
//! human, we do not approve — that is the whole point of having a handler in
//! the first place.

use std::io::{self, BufRead, Write};

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::approval::ApprovalHandler;

/// `ApprovalHandler` that prompts on stdout / reads from stdin.
pub struct TerminalApprovalHandler {
    /// Serialises prompts so parallel agent steps don't interleave their
    /// questions on the terminal.
    lock: Mutex<()>,
}

impl TerminalApprovalHandler {
    pub fn new() -> Self {
        Self {
            lock: Mutex::new(()),
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

        tokio::task::spawn_blocking(move || prompt_blocking(&description, explanation.as_deref()))
            .await
            .map_err(|e| format!("approval prompt task failed: {e}"))?
    }
}

fn prompt_blocking(description: &str, explanation: Option<&str>) -> Result<bool, String> {
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
