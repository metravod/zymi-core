use std::sync::Arc;

use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::policy::{PolicyDecision, PolicyEngine};

use super::{Intention, IntentionVerdict};

/// Configuration for file write boundary contracts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RateLimitConfig {
    /// Maximum shell commands per minute. 0 = unlimited.
    #[serde(default)]
    pub shell_commands_per_minute: u32,
    /// Maximum web requests per minute. 0 = unlimited.
    #[serde(default)]
    pub web_requests_per_minute: u32,
}

/// Built-in deny patterns applied to **both** file reads and writes, on top of
/// any configured `deny_patterns`. Secrets must never be readable into the
/// append-only event log (a `ToolCallCompleted.result` is persisted forever and
/// fed to the LLM), nor writable. Secure-by-default: this holds even when the
/// user configures no `deny_patterns` of their own. See ADR-0036.
const BUILTIN_SECRET_DENY: &[&str] = &[
    "*.env",
    ".env",
    ".env.*",
    "*.key",
    "*.pem",
    "*.pfx",
    "*.p12",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    ".netrc",
    ".pypirc",
    ".npmrc",
    "credentials",
    "*.secret",
];

/// Boundary contract engine. Evaluates agent intentions against configured
/// contracts before allowing execution.
///
/// For shell commands, delegates to the existing PolicyEngine.
/// For file reads and writes, applies path-based contracts: a built-in +
/// configured secret deny-list (reads and writes), and an `allowed_dirs`
/// boundary (writes). Paths are lexically normalized before matching so
/// `..` traversal cannot escape the boundary.
/// Extensible with rate limits and other contract types.
pub struct ContractEngine {
    shell_policy: Arc<PolicyEngine>,
    file_contract: FileWriteContract,
    deny_regexes: Vec<Regex>,
}

impl ContractEngine {
    pub fn new(shell_policy: Arc<PolicyEngine>, file_contract: FileWriteContract) -> Self {
        let deny_regexes = BUILTIN_SECRET_DENY
            .iter()
            .map(|p| (*p).to_string())
            .chain(file_contract.deny_patterns.iter().cloned())
            .filter_map(|p| glob_to_regex(&p))
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
            Intention::ReadFile { path } => self.evaluate_file_read(path),
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

    /// Reads are permissive by default, but the secret deny-list (built-in +
    /// configured) always applies: a denied file must never be read into the
    /// event log or handed to the LLM.
    fn evaluate_file_read(&self, path: &str) -> IntentionVerdict {
        if let Some(reason) = self.denied_by_pattern(path) {
            return IntentionVerdict::Denied {
                reason: format!("File read denied ({reason}): {path}"),
            };
        }
        IntentionVerdict::Approved
    }

    fn evaluate_file_write(&self, path: &str) -> IntentionVerdict {
        if let Some(reason) = self.denied_by_pattern(path) {
            return IntentionVerdict::Denied {
                reason: format!("File write denied ({reason}): {path}"),
            };
        }

        if self.file_contract.allowed_dirs.is_empty() {
            return IntentionVerdict::RequiresHumanApproval {
                reason: format!("No allowed_dirs configured, file write requires approval: {path}"),
            };
        }

        // Compare on lexically-normalized components so `..` traversal cannot
        // smuggle a write out of an allowed dir (e.g. `./memory/../../etc`).
        let target = normalize_components(path);
        for dir in &self.file_contract.allowed_dirs {
            if is_within(&target, &normalize_components(dir)) {
                return IntentionVerdict::Approved;
            }
        }

        IntentionVerdict::RequiresHumanApproval {
            reason: format!("File write outside allowed directories: {path}"),
        }
    }

    /// Returns `Some(pattern)` if the path (or its basename) matches any deny
    /// regex. Matches on the normalized path and the last component so both
    /// path-style (`secrets/*`) and name-style (`.env`, `*.key`) patterns work,
    /// regardless of `..` traversal.
    fn denied_by_pattern(&self, path: &str) -> Option<String> {
        let normalized = normalize_string(path);
        let basename = normalized.rsplit('/').next().unwrap_or(&normalized);
        for regex in &self.deny_regexes {
            if regex.is_match(&normalized) || regex.is_match(basename) {
                return Some(regex.as_str().to_string());
            }
        }
        None
    }
}

/// Lexically normalize a path into components, resolving `.` and `..` without
/// touching the filesystem (the target may not exist yet, and we must not
/// follow symlinks here). Note: symlinks are *not* resolved — a symlink inside
/// an allowed dir pointing elsewhere is out of scope for this lexical check
/// (see ADR-0036).
fn normalize_components(path: &str) -> Vec<String> {
    let is_abs = path.starts_with('/');
    let mut out: Vec<String> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => match out.last().map(String::as_str) {
                Some(top) if top != ".." => {
                    out.pop();
                }
                _ if !is_abs => out.push("..".into()),
                _ => {} // `..` above filesystem root is a no-op
            },
            other => out.push(other.to_string()),
        }
    }
    out
}

/// Normalized path as a string, preserving a leading `/` for absolute paths.
fn normalize_string(path: &str) -> String {
    let prefix = if path.starts_with('/') { "/" } else { "" };
    format!("{prefix}{}", normalize_components(path).join("/"))
}

/// True if `target`'s components are prefixed by `dir`'s components — i.e. the
/// target lies within `dir`. An empty `dir` (e.g. `./`) contains everything.
fn is_within(target: &[String], dir: &[String]) -> bool {
    target.len() >= dir.len() && target.iter().zip(dir).all(|(a, b)| a == b)
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
    fn read_non_secret_approved() {
        let policy = make_policy(vec![], vec![], vec![]);
        let engine = ContractEngine::new(policy, FileWriteContract::default());

        let verdict = engine.evaluate(&Intention::ReadFile {
            path: "./docs/notes.md".into(),
        });
        assert_eq!(verdict, IntentionVerdict::Approved);
    }

    #[test]
    fn read_secret_denied_by_default() {
        // Secure by default: no deny_patterns configured, but the built-in
        // secret list still blocks reading .env / private keys into the log.
        let policy = make_policy(vec![], vec![], vec![]);
        let engine = ContractEngine::new(policy, FileWriteContract::default());

        for p in [".env", "./config/prod.env", "/home/u/.ssh/id_rsa", "app.key"] {
            let verdict = engine.evaluate(&Intention::ReadFile { path: p.into() });
            assert!(
                matches!(verdict, IntentionVerdict::Denied { .. }),
                "expected {p} to be denied, got {verdict:?}"
            );
        }
    }

    #[test]
    fn write_traversal_cannot_escape_allowed_dir() {
        let policy = make_policy(vec![], vec![], vec![]);
        let contract = FileWriteContract {
            allowed_dirs: vec!["./memory/".into()],
            deny_patterns: vec![],
        };
        let engine = ContractEngine::new(policy, contract);

        // `..` traversal out of the allowed dir must NOT auto-approve.
        let verdict = engine.evaluate(&Intention::WriteFile {
            path: "./memory/../../home/user/.ssh/config".into(),
            content: "pwn".into(),
        });
        assert!(
            matches!(verdict, IntentionVerdict::RequiresHumanApproval { .. }),
            "traversal escape should require approval, got {verdict:?}"
        );

        // A genuine in-dir write still approves.
        let ok = engine.evaluate(&Intention::WriteFile {
            path: "./memory/notes.md".into(),
            content: "hi".into(),
        });
        assert_eq!(ok, IntentionVerdict::Approved);
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
