pub mod contracts;
pub mod orchestrator;
pub mod projections;

use serde::{Deserialize, Serialize};

/// An intention represents a side-effect the agent wants to perform.
/// In the ESAA model, agents are intention emitters — they don't execute
/// side effects directly. Instead, intentions are evaluated by the
/// ContractEngine and executed by the Orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Intention {
    /// Execute a shell command on the host.
    ExecuteShellCommand {
        command: String,
        timeout_secs: Option<u64>,
    },

    /// Write content to a file on the host filesystem.
    WriteFile {
        path: String,
        content: String,
    },

    /// Read a file from the host filesystem.
    ReadFile {
        path: String,
    },

    /// Perform a web search.
    WebSearch {
        query: String,
    },

    /// Scrape a URL.
    WebScrape {
        url: String,
    },

    /// Write to the agent's memory store.
    WriteMemory {
        key: String,
        content: String,
    },

    /// Spawn a sub-agent.
    SpawnSubAgent {
        name: String,
        task: String,
    },

    /// Call a user-defined Python tool registered via @tool decorator.
    CallCustomTool {
        tool_name: String,
        arguments: String,
    },
}

impl Intention {
    /// Short tag for logging and event indexing.
    pub fn tag(&self) -> &'static str {
        match self {
            Intention::ExecuteShellCommand { .. } => "execute_shell_command",
            Intention::WriteFile { .. } => "write_file",
            Intention::ReadFile { .. } => "read_file",
            Intention::WebSearch { .. } => "web_search",
            Intention::WebScrape { .. } => "web_scrape",
            Intention::WriteMemory { .. } => "write_memory",
            Intention::SpawnSubAgent { .. } => "spawn_sub_agent",
            Intention::CallCustomTool { .. } => "call_custom_tool",
        }
    }
}

/// The verdict after evaluating an intention against boundary contracts.
#[derive(Debug, Clone, PartialEq)]
pub enum IntentionVerdict {
    /// The intention is within boundary contracts and may proceed.
    Approved,
    /// The intention requires explicit human approval before execution.
    RequiresHumanApproval { reason: String },
    /// The intention violates a boundary contract and must not be executed.
    Denied { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intention_serialization_roundtrip() {
        let intention = Intention::ExecuteShellCommand {
            command: "ls -la".into(),
            timeout_secs: Some(30),
        };
        let json = serde_json::to_string(&intention).unwrap();
        let deserialized: Intention = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.tag(), "execute_shell_command");
    }

    #[test]
    fn intention_tags() {
        assert_eq!(Intention::WriteFile { path: "a".into(), content: "b".into() }.tag(), "write_file");
        assert_eq!(Intention::ReadFile { path: "a".into() }.tag(), "read_file");
        assert_eq!(Intention::WebSearch { query: "q".into() }.tag(), "web_search");
        assert_eq!(Intention::WriteMemory { key: "k".into(), content: "v".into() }.tag(), "write_memory");
        assert_eq!(Intention::CallCustomTool { tool_name: "search".into(), arguments: "{}".into() }.tag(), "call_custom_tool");
    }

    #[test]
    fn verdict_equality() {
        assert_eq!(IntentionVerdict::Approved, IntentionVerdict::Approved);
        assert_ne!(
            IntentionVerdict::Approved,
            IntentionVerdict::Denied { reason: "test".into() }
        );
    }
}
