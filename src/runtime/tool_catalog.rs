//! Tool catalog — the single registry of all tools available to agents.
//!
//! Slice 1 of declarative custom tools (ADR-0014). The catalog answers
//! three questions for any tool name, regardless of its origin (builtin,
//! declarative YAML, programmatic Python/@tool):
//!
//! 1. **`knows(name)`** — is this a valid tool? Used by the config validator
//!    instead of the old `KNOWN_TOOLS` static slice.
//! 2. **`definition(name)`** — produce a [`ToolDefinition`] for the LLM's
//!    `ChatRequest.tools`, replacing `tool_definitions_for_agent`.
//! 3. **`intention(name, args)`** + **`requires_approval(name)`** — map a tool
//!    call to an [`Intention`] for ESAA evaluation.
//!
//! Lookup precedence: **builtin > declarative > programmatic**. Name
//! collisions between layers are a hard error at construction time.

use std::collections::HashMap;

use crate::config::tool::{ImplementationConfig, ToolConfig};
use crate::config::validate::ToolNameResolver;
use crate::esaa::Intention;
use crate::mcp::{self, McpTool};
use crate::types::ToolDefinition;

/// Entry for a single built-in tool.
struct BuiltinEntry {
    definition: ToolDefinition,
    /// Build an [`Intention`] from raw JSON arguments.
    to_intention: fn(&str) -> Intention,
    requires_approval: bool,
}

impl std::fmt::Debug for BuiltinEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltinEntry")
            .field("definition", &self.definition.name)
            .field("requires_approval", &self.requires_approval)
            .finish()
    }
}

/// Entry for a declarative tool loaded from `tools/*.yml`.
#[derive(Debug)]
pub(crate) struct DeclarativeEntry {
    pub(crate) definition: ToolDefinition,
    pub(crate) config: ToolConfig,
}

/// Entry for an MCP-backed tool. Keyed by the full `mcp__server__tool` id.
#[derive(Debug, Clone)]
pub(crate) struct McpEntry {
    pub(crate) definition: ToolDefinition,
    pub(crate) server: String,
    pub(crate) tool: String,
    pub(crate) requires_approval: bool,
}

/// Entry for an auto-discovered Python tool (ADR-0014 slice 3). Lives
/// only when the `python` feature is on; the runtime walks
/// `<project>/tools/*.py` at startup and registers every
/// `@zymi.tool`-decorated callable here.
#[cfg(feature = "python")]
pub(crate) struct PythonEntry {
    pub(crate) definition: ToolDefinition,
    pub(crate) callable: pyo3::Py<pyo3::PyAny>,
    pub(crate) is_async: bool,
    pub(crate) requires_approval: bool,
    pub(crate) source: std::path::PathBuf,
}

#[cfg(feature = "python")]
impl std::fmt::Debug for PythonEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PythonEntry")
            .field("name", &self.definition.name)
            .field("is_async", &self.is_async)
            .field("requires_approval", &self.requires_approval)
            .field("source", &self.source)
            .finish()
    }
}

/// The tool catalog. Constructed once per [`super::Runtime`] and shared by
/// the config validator, the pipeline handler, and the action executor.
#[derive(Debug)]
pub struct ToolCatalog {
    builtin: HashMap<String, BuiltinEntry>,
    declarative: HashMap<String, DeclarativeEntry>,
    mcp: HashMap<String, McpEntry>,
    #[cfg(feature = "python")]
    python: HashMap<String, PythonEntry>,
}

impl ToolCatalog {
    /// Build a catalog seeded with the seven built-in tools.
    pub fn builtin_only() -> Self {
        let mut builtin = HashMap::new();

        register_builtin(&mut builtin, "execute_shell_command",
            "Execute a shell command on the host",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command to run"},
                    "timeout_secs": {"type": "integer", "description": "Timeout in seconds (default 30)"}
                },
                "required": ["command"]
            }),
            |args_json| {
                let args: serde_json::Value = serde_json::from_str(args_json).unwrap_or_default();
                Intention::ExecuteShellCommand {
                    command: args.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    timeout_secs: args.get("timeout_secs").and_then(|v| v.as_u64()),
                }
            },
            false, // approval is handled by ContractEngine's PolicyEngine
        );

        register_builtin(&mut builtin, "write_file",
            "Write content to a file",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path from project root"},
                    "content": {"type": "string", "description": "File content to write"}
                },
                "required": ["path", "content"]
            }),
            |args_json| {
                let args: serde_json::Value = serde_json::from_str(args_json).unwrap_or_default();
                Intention::WriteFile {
                    path: args.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    content: args.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                }
            },
            false,
        );

        register_builtin(&mut builtin, "read_file",
            "Read a file's contents",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path from project root"}
                },
                "required": ["path"]
            }),
            |args_json| {
                let args: serde_json::Value = serde_json::from_str(args_json).unwrap_or_default();
                Intention::ReadFile {
                    path: args.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                }
            },
            false,
        );

        register_builtin(&mut builtin, "write_memory",
            "Store a key-value pair in the agent's memory",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Memory key"},
                    "content": {"type": "string", "description": "Content to store"}
                },
                "required": ["key", "content"]
            }),
            |args_json| {
                let args: serde_json::Value = serde_json::from_str(args_json).unwrap_or_default();
                Intention::WriteMemory {
                    key: args.get("key").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    content: args.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                }
            },
            false,
        );

        register_builtin(&mut builtin, "spawn_sub_agent",
            "Spawn a sub-agent to handle a subtask",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Sub-agent name"},
                    "task": {"type": "string", "description": "Task description for the sub-agent"}
                },
                "required": ["name", "task"]
            }),
            |args_json| {
                let args: serde_json::Value = serde_json::from_str(args_json).unwrap_or_default();
                Intention::SpawnSubAgent {
                    name: args.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    task: args.get("task").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                }
            },
            false,
        );

        Self {
            builtin,
            declarative: HashMap::new(),
            mcp: HashMap::new(),
            #[cfg(feature = "python")]
            python: HashMap::new(),
        }
    }

    /// Build a catalog from built-ins plus declarative tool configs.
    /// Returns an error if a declarative tool name collides with a built-in.
    pub fn with_declarative(
        tools: &HashMap<String, ToolConfig>,
    ) -> Result<Self, String> {
        let mut catalog = Self::builtin_only();

        for (name, config) in tools {
            if catalog.builtin.contains_key(name) {
                return Err(format!(
                    "declarative tool '{}' collides with built-in tool of the same name",
                    name
                ));
            }

            let definition = ToolDefinition {
                name: config.name.clone(),
                description: config.description.clone(),
                parameters: config.parameters.clone(),
            };

            catalog.declarative.insert(
                name.clone(),
                DeclarativeEntry {
                    definition,
                    config: config.clone(),
                },
            );
        }

        Ok(catalog)
    }

    /// Register a batch of MCP tools advertised by `server` (typically the
    /// result of [`crate::mcp::McpServerConnection::list_tools`] after applying
    /// allow/deny filters one layer up).
    ///
    /// Each tool is registered under the catalog id `mcp__<server>__<tool>`.
    /// Returns an error if the server name or any tool name contains the
    /// reserved `__` separator, or if a generated id collides with an
    /// existing entry. `requires_approval` becomes the default for every
    /// tool from this server (per-server policy lives one layer up).
    pub fn add_mcp_server(
        &mut self,
        server: &str,
        tools: &[McpTool],
        requires_approval: bool,
    ) -> Result<(), String> {
        mcp::validate_segment(server).map_err(|e| format!("mcp server name: {e}"))?;
        for tool in tools {
            mcp::validate_segment(&tool.name)
                .map_err(|e| format!("mcp tool '{}': {e}", tool.name))?;
            let id = mcp::make_id(server, &tool.name);
            if self.builtin.contains_key(&id)
                || self.declarative.contains_key(&id)
                || self.mcp.contains_key(&id)
            {
                return Err(format!("mcp tool id '{id}' collides with existing entry"));
            }
            let definition = ToolDefinition {
                name: id.clone(),
                description: tool.description.clone().unwrap_or_default(),
                parameters: if tool.input_schema.is_null() {
                    serde_json::json!({"type": "object", "properties": {}})
                } else {
                    tool.input_schema.clone()
                },
            };
            self.mcp.insert(
                id,
                McpEntry {
                    definition,
                    server: server.to_string(),
                    tool: tool.name.clone(),
                    requires_approval,
                },
            );
        }
        Ok(())
    }

    /// Register a batch of auto-discovered Python tools. Behind the
    /// `python` feature. Returns an error if a name collides with an
    /// existing builtin / declarative / MCP / Python entry.
    #[cfg(feature = "python")]
    pub fn add_python_tools(
        &mut self,
        tools: Vec<crate::python::auto_discover::DiscoveredTool>,
        default_requires_approval: bool,
    ) -> Result<(), String> {
        for tool in tools {
            if self.builtin.contains_key(&tool.name)
                || self.declarative.contains_key(&tool.name)
                || self.mcp.contains_key(&tool.name)
                || self.python.contains_key(&tool.name)
            {
                return Err(format!(
                    "python tool '{}' (from {}) collides with an existing tool",
                    tool.name,
                    tool.source.display()
                ));
            }
            let definition = ToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            };
            let requires_approval = tool.requires_approval.unwrap_or(default_requires_approval);
            self.python.insert(
                tool.name.clone(),
                PythonEntry {
                    definition,
                    callable: tool.callable,
                    is_async: tool.is_async,
                    requires_approval,
                    source: tool.source,
                },
            );
        }
        Ok(())
    }

    /// Is this tool name registered in the catalog?
    pub fn knows(&self, name: &str) -> bool {
        let base = self.builtin.contains_key(name)
            || self.declarative.contains_key(name)
            || self.mcp.contains_key(name);
        #[cfg(feature = "python")]
        {
            base || self.python.contains_key(name)
        }
        #[cfg(not(feature = "python"))]
        {
            base
        }
    }

    /// Is this an auto-discovered Python tool?
    #[cfg(feature = "python")]
    pub fn is_python(&self, name: &str) -> bool {
        self.python.contains_key(name)
    }

    /// Borrow the underlying Python entry for dispatch. Internal API.
    #[cfg(feature = "python")]
    pub(crate) fn python_entry(&self, name: &str) -> Option<&PythonEntry> {
        self.python.get(name)
    }

    /// Number of registered Python tools — used in startup logs and the
    /// `tools list` command.
    #[cfg(feature = "python")]
    pub fn python_tool_count(&self) -> usize {
        self.python.len()
    }

    /// Is this a declarative (YAML-defined) tool?
    pub fn is_declarative(&self, name: &str) -> bool {
        self.declarative.contains_key(name)
    }

    /// Is this an MCP-backed tool?
    pub fn is_mcp(&self, name: &str) -> bool {
        self.mcp.contains_key(name)
    }

    /// Resolve an MCP catalog id back to `(server, tool)` for executor
    /// dispatch.
    pub fn mcp_route(&self, name: &str) -> Option<(&str, &str)> {
        let entry = self.mcp.get(name)?;
        Some((entry.server.as_str(), entry.tool.as_str()))
    }

    /// Number of MCP tools registered under a given server name.
    pub fn mcp_tool_count(&self, server: &str) -> usize {
        self.mcp
            .values()
            .filter(|entry| entry.server == server)
            .count()
    }

    /// Get the [`ToolConfig`] for a declarative tool.
    pub(crate) fn declarative_config(&self, name: &str) -> Option<&ToolConfig> {
        self.declarative.get(name).map(|e| &e.config)
    }

    /// Get the [`ToolDefinition`] for a tool (sent to the LLM).
    pub fn definition(&self, name: &str) -> Option<&ToolDefinition> {
        let core = self
            .builtin
            .get(name)
            .map(|e| &e.definition)
            .or_else(|| self.declarative.get(name).map(|e| &e.definition))
            .or_else(|| self.mcp.get(name).map(|e| &e.definition));
        #[cfg(feature = "python")]
        {
            core.or_else(|| self.python.get(name).map(|e| &e.definition))
        }
        #[cfg(not(feature = "python"))]
        {
            core
        }
    }

    /// Build the tool definition list for an agent's declared tool names.
    pub fn definitions_for_agent(&self, tools: &[String]) -> Vec<ToolDefinition> {
        tools
            .iter()
            .filter_map(|name| self.definition(name))
            .cloned()
            .collect()
    }

    /// Map a tool call to an [`Intention`] for ESAA evaluation.
    ///
    /// Declarative `kind: shell` tools map to
    /// [`Intention::ExecuteShellCommand`] (same gate as the built-in) with
    /// the resolved command template. All other declarative tools map to
    /// [`Intention::CallCustomTool`].
    pub fn intention(&self, name: &str, arguments_json: &str) -> Intention {
        if let Some(entry) = self.builtin.get(name) {
            return (entry.to_intention)(arguments_json);
        }

        if let Some(entry) = self.declarative.get(name) {
            if let ImplementationConfig::Shell {
                command_template,
                timeout_secs,
            } = &entry.config.implementation
            {
                let args = crate::runtime::action_executor::parse_args_for_interpolation(
                    arguments_json,
                );
                let resolved =
                    crate::runtime::action_executor::resolve_args(command_template, &args);
                return Intention::ExecuteShellCommand {
                    command: resolved,
                    timeout_secs: *timeout_secs,
                };
            }
        }

        Intention::CallCustomTool {
            tool_name: name.to_string(),
            arguments: arguments_json.to_string(),
        }
    }

    /// Does this tool require human approval by default?
    pub fn requires_approval(&self, name: &str) -> bool {
        if let Some(entry) = self.builtin.get(name) {
            return entry.requires_approval;
        }
        if let Some(entry) = self.declarative.get(name) {
            return entry.config.effective_requires_approval();
        }
        if let Some(entry) = self.mcp.get(name) {
            return entry.requires_approval;
        }
        #[cfg(feature = "python")]
        if let Some(entry) = self.python.get(name) {
            return entry.requires_approval;
        }
        false
    }

    /// All known tool names (for error messages / help text).
    pub fn all_tool_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.builtin.keys().map(|s| s.as_str()).collect();
        names.extend(self.declarative.keys().map(|s| s.as_str()));
        names.extend(self.mcp.keys().map(|s| s.as_str()));
        #[cfg(feature = "python")]
        names.extend(self.python.keys().map(|s| s.as_str()));
        names.sort_unstable();
        names
    }
}

impl ToolNameResolver for ToolCatalog {
    fn knows(&self, name: &str) -> bool {
        self.knows(name)
    }

    fn all_tool_names(&self) -> Vec<&str> {
        self.all_tool_names()
    }
}

fn register_builtin(
    map: &mut HashMap<String, BuiltinEntry>,
    name: &str,
    description: &str,
    parameters: serde_json::Value,
    to_intention: fn(&str) -> Intention,
    requires_approval: bool,
) {
    map.insert(
        name.to_string(),
        BuiltinEntry {
            definition: ToolDefinition {
                name: name.to_string(),
                description: description.to_string(),
                parameters,
            },
            to_intention,
            requires_approval,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tool::{ImplementationConfig, HttpMethod};

    fn make_http_tool(name: &str) -> ToolConfig {
        ToolConfig {
            name: name.to_string(),
            description: format!("Test tool {name}"),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            requires_approval: None,
            implementation: ImplementationConfig::Http {
                method: HttpMethod::Post,
                url: "https://example.com".into(),
                headers: HashMap::new(),
                body_template: None,
            },
        }
    }

    #[test]
    fn builtin_catalog_knows_all_five() {
        let catalog = ToolCatalog::builtin_only();
        let expected = [
            "execute_shell_command",
            "write_file",
            "read_file",
            "write_memory",
            "spawn_sub_agent",
        ];
        for name in &expected {
            assert!(catalog.knows(name), "catalog should know {name}");
            assert!(catalog.definition(name).is_some(), "catalog should have definition for {name}");
        }
        assert_eq!(catalog.all_tool_names().len(), 5);
    }

    #[test]
    fn unknown_tool_not_known() {
        let catalog = ToolCatalog::builtin_only();
        assert!(!catalog.knows("my_custom_tool"));
        assert!(catalog.definition("my_custom_tool").is_none());
    }

    #[test]
    fn definitions_for_agent_filters_correctly() {
        let catalog = ToolCatalog::builtin_only();
        let tools = vec!["write_memory".into(), "nonexistent".into(), "read_file".into()];
        let defs = catalog.definitions_for_agent(&tools);
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "write_memory");
        assert_eq!(defs[1].name, "read_file");
    }

    #[test]
    fn intention_builtin_maps_correctly() {
        let catalog = ToolCatalog::builtin_only();
        let intention = catalog.intention("execute_shell_command", r#"{"command":"ls -la"}"#);
        assert_eq!(intention.tag(), "execute_shell_command");

        let intention = catalog.intention("write_file", r#"{"path":"a.txt","content":"hello"}"#);
        assert_eq!(intention.tag(), "write_file");
    }

    #[test]
    fn intention_unknown_falls_to_custom() {
        let catalog = ToolCatalog::builtin_only();
        let intention = catalog.intention("my_api", r#"{"key":"val"}"#);
        assert_eq!(intention.tag(), "call_custom_tool");
    }

    #[test]
    fn requires_approval_defaults() {
        let catalog = ToolCatalog::builtin_only();
        assert!(!catalog.requires_approval("execute_shell_command"));
        assert!(!catalog.requires_approval("write_file"));
        assert!(!catalog.requires_approval("unknown_tool"));
    }

    #[test]
    fn declarative_tool_registered() {
        let mut tools = HashMap::new();
        tools.insert("slack_post".into(), make_http_tool("slack_post"));

        let catalog = ToolCatalog::with_declarative(&tools).unwrap();
        assert!(catalog.knows("slack_post"));
        assert!(catalog.is_declarative("slack_post"));
        assert_eq!(catalog.all_tool_names().len(), 6); // 5 builtin + 1

        let def = catalog.definition("slack_post").unwrap();
        assert_eq!(def.name, "slack_post");
    }

    #[test]
    fn declarative_tool_in_agent_definitions() {
        let mut tools = HashMap::new();
        tools.insert("my_api".into(), make_http_tool("my_api"));
        let catalog = ToolCatalog::with_declarative(&tools).unwrap();

        let defs = catalog.definitions_for_agent(&["read_file".into(), "my_api".into()]);
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[1].name, "my_api");
    }

    #[test]
    fn declarative_intention_is_call_custom() {
        let mut tools = HashMap::new();
        tools.insert("my_api".into(), make_http_tool("my_api"));
        let catalog = ToolCatalog::with_declarative(&tools).unwrap();

        let intention = catalog.intention("my_api", r#"{"key":"val"}"#);
        assert_eq!(intention.tag(), "call_custom_tool");
    }

    #[test]
    fn declarative_requires_approval_http_default() {
        let mut tools = HashMap::new();
        tools.insert("my_api".into(), make_http_tool("my_api"));
        let catalog = ToolCatalog::with_declarative(&tools).unwrap();
        assert!(!catalog.requires_approval("my_api"));
    }

    #[test]
    fn declarative_requires_approval_override() {
        let mut tools = HashMap::new();
        let mut tool = make_http_tool("sensitive");
        tool.requires_approval = Some(true);
        tools.insert("sensitive".into(), tool);

        let catalog = ToolCatalog::with_declarative(&tools).unwrap();
        assert!(catalog.requires_approval("sensitive"));
    }

    #[test]
    fn builtin_name_collision_is_error() {
        let mut tools = HashMap::new();
        tools.insert("read_file".into(), make_http_tool("read_file"));

        let err = ToolCatalog::with_declarative(&tools).unwrap_err();
        assert!(err.contains("collides with built-in"));
    }

    #[test]
    fn web_search_declarative_no_collision() {
        let mut tools = HashMap::new();
        tools.insert("web_search".into(), make_http_tool("web_search"));

        let catalog = ToolCatalog::with_declarative(&tools).unwrap();
        assert!(catalog.knows("web_search"));
        assert!(catalog.is_declarative("web_search"));
    }

    #[test]
    fn web_scrape_declarative_no_collision() {
        let mut tools = HashMap::new();
        tools.insert("web_scrape".into(), make_http_tool("web_scrape"));

        let catalog = ToolCatalog::with_declarative(&tools).unwrap();
        assert!(catalog.knows("web_scrape"));
        assert!(catalog.is_declarative("web_scrape"));
    }

    fn make_shell_tool(name: &str, command_template: &str) -> ToolConfig {
        ToolConfig {
            name: name.to_string(),
            description: format!("Shell tool {name}"),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "dir": {"type": "string"}
                },
                "required": ["dir"]
            }),
            requires_approval: None,
            implementation: ImplementationConfig::Shell {
                command_template: command_template.to_string(),
                timeout_secs: Some(60),
            },
        }
    }

    #[test]
    fn shell_tool_registered_and_known() {
        let mut tools = HashMap::new();
        tools.insert(
            "run_tests".into(),
            make_shell_tool("run_tests", "cd ${args.dir} && cargo test"),
        );
        let catalog = ToolCatalog::with_declarative(&tools).unwrap();
        assert!(catalog.knows("run_tests"));
        assert!(catalog.is_declarative("run_tests"));
        assert_eq!(catalog.all_tool_names().len(), 6); // 5 builtin + 1 shell
    }

    #[test]
    fn shell_tool_requires_approval_by_default() {
        let mut tools = HashMap::new();
        tools.insert("sh".into(), make_shell_tool("sh", "echo hi"));
        let catalog = ToolCatalog::with_declarative(&tools).unwrap();
        assert!(catalog.requires_approval("sh"));
    }

    #[test]
    fn shell_tool_intention_maps_to_execute_shell_command() {
        let mut tools = HashMap::new();
        tools.insert(
            "run_tests".into(),
            make_shell_tool("run_tests", "cd ${args.dir} && cargo test"),
        );
        let catalog = ToolCatalog::with_declarative(&tools).unwrap();

        let intention = catalog.intention("run_tests", r#"{"dir":"src"}"#);
        assert_eq!(intention.tag(), "execute_shell_command");

        // Verify the command was resolved.
        if let Intention::ExecuteShellCommand { command, timeout_secs } = &intention {
            assert_eq!(command, "cd src && cargo test");
            assert_eq!(*timeout_secs, Some(60));
        } else {
            panic!("expected ExecuteShellCommand");
        }
    }

    #[test]
    fn http_tool_intention_stays_call_custom() {
        let mut tools = HashMap::new();
        tools.insert("api".into(), make_http_tool("api"));
        let catalog = ToolCatalog::with_declarative(&tools).unwrap();

        let intention = catalog.intention("api", r#"{"key":"val"}"#);
        assert_eq!(intention.tag(), "call_custom_tool");
    }

    fn mcp_tool(name: &str) -> McpTool {
        McpTool {
            name: name.into(),
            description: Some(format!("desc for {name}")),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    #[test]
    fn mcp_server_tools_register_with_namespaced_id() {
        let mut catalog = ToolCatalog::builtin_only();
        catalog
            .add_mcp_server("github", &[mcp_tool("create_issue"), mcp_tool("list_repos")], false)
            .unwrap();

        assert!(catalog.knows("mcp__github__create_issue"));
        assert!(catalog.knows("mcp__github__list_repos"));
        assert!(catalog.is_mcp("mcp__github__create_issue"));
        assert!(!catalog.is_declarative("mcp__github__create_issue"));

        let def = catalog.definition("mcp__github__create_issue").unwrap();
        assert_eq!(def.name, "mcp__github__create_issue");
        assert_eq!(def.description, "desc for create_issue");
    }

    #[test]
    fn mcp_route_returns_server_and_original_tool_name() {
        let mut catalog = ToolCatalog::builtin_only();
        catalog
            .add_mcp_server("postgres", &[mcp_tool("query")], false)
            .unwrap();

        let (server, tool) = catalog.mcp_route("mcp__postgres__query").unwrap();
        assert_eq!(server, "postgres");
        assert_eq!(tool, "query");
        assert!(catalog.mcp_route("write_file").is_none());
    }

    #[test]
    fn mcp_definitions_for_agent_include_full_id() {
        let mut catalog = ToolCatalog::builtin_only();
        catalog.add_mcp_server("gh", &[mcp_tool("issue")], false).unwrap();

        let defs = catalog.definitions_for_agent(&["mcp__gh__issue".into()]);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "mcp__gh__issue");
    }

    #[test]
    fn mcp_intention_routes_through_call_custom_with_full_id() {
        let mut catalog = ToolCatalog::builtin_only();
        catalog.add_mcp_server("gh", &[mcp_tool("issue")], false).unwrap();

        let intention = catalog.intention("mcp__gh__issue", r#"{"title":"x"}"#);
        match intention {
            Intention::CallCustomTool { tool_name, arguments } => {
                assert_eq!(tool_name, "mcp__gh__issue");
                assert_eq!(arguments, r#"{"title":"x"}"#);
            }
            other => panic!("expected CallCustomTool, got {other:?}"),
        }
    }

    #[test]
    fn mcp_requires_approval_uses_per_server_default() {
        let mut catalog = ToolCatalog::builtin_only();
        catalog.add_mcp_server("safe", &[mcp_tool("read")], false).unwrap();
        catalog.add_mcp_server("risky", &[mcp_tool("write")], true).unwrap();

        assert!(!catalog.requires_approval("mcp__safe__read"));
        assert!(catalog.requires_approval("mcp__risky__write"));
    }

    #[test]
    fn mcp_double_underscore_in_segment_rejected() {
        let mut catalog = ToolCatalog::builtin_only();
        let err = catalog
            .add_mcp_server("evil__server", &[mcp_tool("ok")], false)
            .unwrap_err();
        assert!(err.contains("__"), "got: {err}");

        let err = catalog
            .add_mcp_server("ok", &[mcp_tool("nested__name")], false)
            .unwrap_err();
        assert!(err.contains("__"), "got: {err}");
    }

    #[test]
    fn mcp_id_collision_is_rejected() {
        let mut catalog = ToolCatalog::builtin_only();
        catalog.add_mcp_server("gh", &[mcp_tool("issue")], false).unwrap();
        let err = catalog
            .add_mcp_server("gh", &[mcp_tool("issue")], false)
            .unwrap_err();
        assert!(err.contains("collides"), "got: {err}");
    }

    #[test]
    fn mcp_tools_with_null_input_schema_get_default_object_schema() {
        let mut catalog = ToolCatalog::builtin_only();
        let tool = McpTool {
            name: "ping".into(),
            description: None,
            input_schema: serde_json::Value::Null,
        };
        catalog.add_mcp_server("svc", &[tool], false).unwrap();

        let def = catalog.definition("mcp__svc__ping").unwrap();
        assert_eq!(def.parameters["type"], "object");
    }
}
