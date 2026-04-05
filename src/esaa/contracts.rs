use std::sync::Arc;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::policy::{PolicyDecision, PolicyEngine};

use super::{Intention, IntentionVerdict};

/// Configuration for file write boundary contracts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileWriteContract {
    /// Directory prefixes where writes are allowed (e.g., "/tmp/", "./memory/").
    /// Empty means all writes require approval.
    #[serde(default)]
    pub allowed_dirs: Vec<String>,
    /// Glob patterns for paths that are always denied (e.g., "*.env", "*.key").
    #[serde(default)]
    pub deny_patterns: Vec<String>,
}

/// Configuration for rate limiting intentions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Maximum shell commands per minute. 0 = unlimited.
    #[serde(default)]
    pub shell_commands_per_minute: u32,
    /// Maximum web requests per minute. 0 = unlimited.
    #[serde(default)]
    pub web_requests_per_minute: u32,
}

/// Boundary contract engine. Evaluates agent intentions against configured
/// contracts before allowing execution.
///
/// For shell commands, delegates to the existing PolicyEngine.
/// For file writes, applies path-based contracts.
/// Extensible with rate limits and other contract types.
pub struct ContractEngine {
    shell_policy: Arc<PolicyEngine>,
    file_contract: FileWriteContract,
    deny_regexes: Vec<Regex>,
}

impl ContractEngine {
    pub fn new(shell_policy: Arc<PolicyEngine>, file_contract: FileWriteContract) -> Self {
        let deny_regexes = file_contract
            .deny_patterns
            .iter()
            .filter_map(|p| glob_to_regex(p))
            .collect();

        Self {
            shell_policy,
            file_contract,
            deny_regexes,
        }
    }

    /// Evaluate an intention against all applicable boundary contracts.
    pub fn evaluate(&self, intention: &Intention) -> IntentionVerdict {
        match intention {
            Intention::ExecuteShellCommand { command, .. } => {
                self.evaluate_shell_command(command)
            }
            Intention::WriteFile { path, .. } => self.evaluate_file_write(path),
            Intention::ReadFile { .. } => IntentionVerdict::Approved,
            Intention::WebSearch { .. } | Intention::WebScrape { .. } => {
                IntentionVerdict::Approved
            }
            Intention::WriteMemory { .. } => IntentionVerdict::Approved,
            Intention::SpawnSubAgent { .. } => IntentionVerdict::RequiresHumanApproval {
                reason: "Sub-agent spawn requires approval".into(),
            },
            Intention::CallCustomTool { .. } => IntentionVerdict::Approved,
        }
    }

    fn evaluate_shell_command(&self, command: &str) -> IntentionVerdict {
        match self.shell_policy.evaluate(command) {
            PolicyDecision::Allow => IntentionVerdict::Approved,
            PolicyDecision::RequireApproval => IntentionVerdict::RequiresHumanApproval {
                reason: format!("Shell command requires approval: {}", truncate(command, 100)),
            },
            PolicyDecision::Deny(reason) => IntentionVerdict::Denied { reason },
        }
    }

    fn evaluate_file_write(&self, path: &str) -> IntentionVerdict {
        for regex in &self.deny_regexes {
            if regex.is_match(path) {
                return IntentionVerdict::Denied {
                    reason: format!("File write denied by pattern: {path}"),
                };
            }
        }

        if self.file_contract.allowed_dirs.is_empty() {
            return IntentionVerdict::RequiresHumanApproval {
                reason: format!("No allowed_dirs configured, file write requires approval: {path}"),
            };
        }

        for dir in &self.file_contract.allowed_dirs {
            if path.starts_with(dir) {
                return IntentionVerdict::Approved;
            }
        }

        IntentionVerdict::RequiresHumanApproval {
            reason: format!("File write outside allowed directories: {path}"),
        }
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let end = s.floor_char_boundary(max);
        &s[..end]
    }
}

/// Convert a simple glob pattern to a regex. Supports `*` and `?`.
fn glob_to_regex(pattern: &str) -> Option<Regex> {
    let escaped = regex::escape(pattern);
    let regex_str = escaped.replace(r"\*", ".*").replace(r"\?", ".");
    Regex::new(&format!("^{regex_str}$")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PolicyConfig;

    fn make_policy(allow: Vec<&str>, deny: Vec<&str>, require: Vec<&str>) -> Arc<PolicyEngine> {
        Arc::new(PolicyEngine::new(PolicyConfig {
            enabled: true,
            allow: allow.into_iter().map(String::from).collect(),
            deny: deny.into_iter().map(String::from).collect(),
            require_approval: require.into_iter().map(String::from).collect(),
        }))
    }

    #[test]
    fn shell_allowed_passes_through() {
        let policy = make_policy(vec!["ls*", "cat*"], vec![], vec![]);
        let engine = ContractEngine::new(policy, FileWriteContract::default());

        let verdict = engine.evaluate(&Intention::ExecuteShellCommand {
            command: "ls -la".into(),
            timeout_secs: None,
        });
        assert_eq!(verdict, IntentionVerdict::Approved);
    }

    #[test]
    fn shell_denied_passes_through() {
        let policy = make_policy(vec!["ls*"], vec!["rm*"], vec![]);
        let engine = ContractEngine::new(policy, FileWriteContract::default());

        let verdict = engine.evaluate(&Intention::ExecuteShellCommand {
            command: "rm -rf /".into(),
            timeout_secs: None,
        });
        assert!(matches!(verdict, IntentionVerdict::Denied { .. }));
    }

    #[test]
    fn shell_require_approval_passes_through() {
        let policy = make_policy(vec!["*"], vec![], vec!["sudo*"]);
        let engine = ContractEngine::new(policy, FileWriteContract::default());

        let verdict = engine.evaluate(&Intention::ExecuteShellCommand {
            command: "sudo apt install".into(),
            timeout_secs: None,
        });
        assert!(matches!(verdict, IntentionVerdict::RequiresHumanApproval { .. }));
    }

    #[test]
    fn file_write_in_allowed_dir() {
        let policy = make_policy(vec![], vec![], vec![]);
        let contract = FileWriteContract {
            allowed_dirs: vec!["./memory/".into(), "/tmp/".into()],
            deny_patterns: vec![],
        };
        let engine = ContractEngine::new(policy, contract);

        let verdict = engine.evaluate(&Intention::WriteFile {
            path: "./memory/notes.md".into(),
            content: "hello".into(),
        });
        assert_eq!(verdict, IntentionVerdict::Approved);
    }

    #[test]
    fn file_write_outside_allowed_dir() {
        let policy = make_policy(vec![], vec![], vec![]);
        let contract = FileWriteContract {
            allowed_dirs: vec!["./memory/".into()],
            deny_patterns: vec![],
        };
        let engine = ContractEngine::new(policy, contract);

        let verdict = engine.evaluate(&Intention::WriteFile {
            path: "/etc/passwd".into(),
            content: "hack".into(),
        });
        assert!(matches!(verdict, IntentionVerdict::RequiresHumanApproval { .. }));
    }

    #[test]
    fn file_write_denied_by_pattern() {
        let policy = make_policy(vec![], vec![], vec![]);
        let contract = FileWriteContract {
            allowed_dirs: vec!["./".into()],
            deny_patterns: vec!["*.env".into(), "*.key".into()],
        };
        let engine = ContractEngine::new(policy, contract);

        let verdict = engine.evaluate(&Intention::WriteFile {
            path: "./.env".into(),
            content: "SECRET=123".into(),
        });
        assert!(matches!(verdict, IntentionVerdict::Denied { .. }));
    }

    #[test]
    fn file_write_no_allowed_dirs_requires_approval() {
        let policy = make_policy(vec![], vec![], vec![]);
        let engine = ContractEngine::new(policy, FileWriteContract::default());

        let verdict = engine.evaluate(&Intention::WriteFile {
            path: "./foo.txt".into(),
            content: "data".into(),
        });
        assert!(matches!(verdict, IntentionVerdict::RequiresHumanApproval { .. }));
    }

    #[test]
    fn read_file_always_approved() {
        let policy = make_policy(vec![], vec![], vec![]);
        let engine = ContractEngine::new(policy, FileWriteContract::default());

        let verdict = engine.evaluate(&Intention::ReadFile {
            path: "/etc/passwd".into(),
        });
        assert_eq!(verdict, IntentionVerdict::Approved);
    }

    #[test]
    fn web_search_approved() {
        let policy = make_policy(vec![], vec![], vec![]);
        let engine = ContractEngine::new(policy, FileWriteContract::default());

        let verdict = engine.evaluate(&Intention::WebSearch {
            query: "rust async".into(),
        });
        assert_eq!(verdict, IntentionVerdict::Approved);
    }

    #[test]
    fn spawn_sub_agent_requires_approval() {
        let policy = make_policy(vec![], vec![], vec![]);
        let engine = ContractEngine::new(policy, FileWriteContract::default());

        let verdict = engine.evaluate(&Intention::SpawnSubAgent {
            name: "researcher".into(),
            task: "find papers".into(),
        });
        assert!(matches!(verdict, IntentionVerdict::RequiresHumanApproval { .. }));
    }

    #[test]
    fn custom_tool_approved() {
        let policy = make_policy(vec![], vec![], vec![]);
        let engine = ContractEngine::new(policy, FileWriteContract::default());

        let verdict = engine.evaluate(&Intention::CallCustomTool {
            tool_name: "search".into(),
            arguments: r#"{"query":"rust"}"#.into(),
        });
        assert_eq!(verdict, IntentionVerdict::Approved);
    }

    #[test]
    fn write_memory_approved() {
        let policy = make_policy(vec![], vec![], vec![]);
        let engine = ContractEngine::new(policy, FileWriteContract::default());

        let verdict = engine.evaluate(&Intention::WriteMemory {
            key: "notes".into(),
            content: "important".into(),
        });
        assert_eq!(verdict, IntentionVerdict::Approved);
    }
}
