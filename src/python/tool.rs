use std::collections::HashMap;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyString};

use crate::esaa::Intention;
use crate::types::ToolDefinition;

/// A registered Python tool callable from the LLM tool-call loop.
struct RegisteredTool {
    definition: ToolDefinition,
    callable: PyObject,
    is_async: bool,
    /// Optional ESAA intention tag. Built-in tools map to Intention variants;
    /// if None, maps to `Intention::CallCustomTool`.
    intention: Option<String>,
}

/// Registry of Python tools. Bridges `@tool`-decorated Python functions
/// to [`ToolDefinition`] for LLM providers and handles invocation.
///
/// Usage from Python:
/// ```python
/// from _zymi_core import ToolRegistry
///
/// registry = ToolRegistry()
///
/// @registry.tool
/// def search(query: str) -> str:
///     """Search the web for information."""
///     return requests.get(f"https://api.search.com?q={query}").text
///
/// @registry.tool(intention="web_search")
/// async def deep_search(query: str, max_results: int = 10) -> str:
///     """Deep search with pagination."""
///     ...
/// ```
#[pyclass(name = "ToolRegistry")]
pub struct PyToolRegistry {
    tools: HashMap<String, RegisteredTool>,
}

#[pymethods]
impl PyToolRegistry {
    #[new]
    fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a Python callable as a tool with explicit metadata.
    ///
    /// Args:
    ///     name: Tool name (what the LLM will call).
    ///     description: Human-readable description for the LLM.
    ///     parameters: JSON Schema dict for function parameters.
    ///     callable: The Python function to invoke.
    ///     intention: Optional ESAA intention tag for contract mapping.
    #[pyo3(signature = (name, description, parameters, callable, intention=None))]
    fn register(
        &mut self,
        py: Python<'_>,
        name: String,
        description: String,
        parameters: &Bound<'_, PyDict>,
        callable: PyObject,
        intention: Option<String>,
    ) -> PyResult<()> {
        let params_json = dict_to_json_value(parameters)?;
        let is_async = check_is_async(py, &callable)?;

        self.tools.insert(
            name.clone(),
            RegisteredTool {
                definition: ToolDefinition {
                    name,
                    description,
                    parameters: params_json,
                },
                callable,
                is_async,
                intention,
            },
        );
        Ok(())
    }

    /// Decorator to register a Python function as a tool.
    ///
    /// Supports two forms:
    /// - `@registry.tool` — infers name, description, and parameters from the function.
    /// - `@registry.tool(description="...", intention="...")` — decorator factory with overrides.
    #[pyo3(signature = (func=None, /, *, description=None, name=None, intention=None))]
    fn tool(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        func: Option<PyObject>,
        description: Option<String>,
        name: Option<String>,
        intention: Option<String>,
    ) -> PyResult<PyObject> {
        match func {
            Some(func) => {
                // Direct decorator: @registry.tool
                slf.borrow_mut().register_from_callable(py, &func, name, description, intention)?;
                Ok(func)
            }
            None => {
                // Decorator factory: @registry.tool(description="...")
                let decorator = Py::new(
                    py,
                    ToolDecorator {
                        registry: slf.clone().unbind(),
                        description,
                        name,
                        intention,
                    },
                )?;
                Ok(decorator.into_py(py))
            }
        }
    }

    /// Invoke a registered tool by name with JSON arguments string.
    ///
    /// Handles both sync and async Python callables. For async functions,
    /// uses `asyncio.run()` to execute the coroutine.
    ///
    /// Returns the result as a string (str() of the return value).
    fn call(&self, py: Python<'_>, name: &str, arguments_json: &str) -> PyResult<String> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| PyErr::new::<pyo3::exceptions::PyKeyError, _>(format!("Unknown tool: {name}")))?;

        let args: serde_json::Value = serde_json::from_str(arguments_json).map_err(|e| {
            PyErr::new::<pyo3::exceptions::PyValueError, _>(format!("Invalid JSON arguments: {e}"))
        })?;

        let kwargs = json_value_to_pydict(py, &args)?;
        let result = tool.callable.call_bound(py, (), Some(&kwargs))?;

        if tool.is_async {
            let resolved = run_coroutine(py, &result)?;
            Ok(resolved.bind(py).str()?.to_string())
        } else {
            Ok(result.bind(py).str()?.to_string())
        }
    }

    /// Get all tool definitions as a list of dicts for the LLM API.
    ///
    /// Each dict has `name`, `description`, and `parameters` keys.
    fn definitions(&self, py: Python<'_>) -> PyResult<Vec<PyObject>> {
        self.tools
            .values()
            .map(|tool| {
                let dict = PyDict::new_bound(py);
                dict.set_item("name", &tool.definition.name)?;
                dict.set_item("description", &tool.definition.description)?;
                let params_dict = json_value_to_pydict(py, &tool.definition.parameters)?;
                dict.set_item("parameters", params_dict)?;
                Ok(dict.into_py(py))
            })
            .collect()
    }

    /// Map a tool call to an ESAA Intention JSON string.
    ///
    /// Built-in tools (execute_shell_command, web_search, etc.) map to their
    /// corresponding Intention variants. Custom tools map to `CallCustomTool`.
    ///
    /// Returns the Intention serialized as JSON.
    fn to_intention(&self, name: &str, arguments_json: &str) -> PyResult<String> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| PyErr::new::<pyo3::exceptions::PyKeyError, _>(format!("Unknown tool: {name}")))?;

        let parse_args = |json: &str| -> PyResult<serde_json::Value> {
            serde_json::from_str(json).map_err(|e| {
                PyErr::new::<pyo3::exceptions::PyValueError, _>(format!(
                    "Invalid JSON arguments for intention '{name}': {e}"
                ))
            })
        };

        let require_str = |args: &serde_json::Value, field: &str| -> PyResult<String> {
            args.get(field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    PyErr::new::<pyo3::exceptions::PyValueError, _>(format!(
                        "Missing or non-string field '{field}' in arguments for intention '{name}'"
                    ))
                })
        };

        let intention = match tool.intention.as_deref() {
            Some("execute_shell_command") => {
                let args = parse_args(arguments_json)?;
                Intention::ExecuteShellCommand {
                    command: require_str(&args, "command")?,
                    timeout_secs: args.get("timeout_secs").and_then(|v| v.as_u64()),
                }
            }
            Some("write_file") => {
                let args = parse_args(arguments_json)?;
                Intention::WriteFile {
                    path: require_str(&args, "path")?,
                    content: require_str(&args, "content")?,
                }
            }
            Some("read_file") => {
                let args = parse_args(arguments_json)?;
                Intention::ReadFile {
                    path: require_str(&args, "path")?,
                }
            }
            Some("web_search") => {
                let args = parse_args(arguments_json)?;
                Intention::WebSearch {
                    query: require_str(&args, "query")?,
                }
            }
            Some("web_scrape") => {
                let args = parse_args(arguments_json)?;
                Intention::WebScrape {
                    url: require_str(&args, "url")?,
                }
            }
            Some("write_memory") => {
                let args = parse_args(arguments_json)?;
                Intention::WriteMemory {
                    key: require_str(&args, "key")?,
                    content: require_str(&args, "content")?,
                }
            }
            Some("spawn_sub_agent") => {
                let args = parse_args(arguments_json)?;
                Intention::SpawnSubAgent {
                    name: require_str(&args, "name")?,
                    task: require_str(&args, "task")?,
                }
            }
            _ => Intention::CallCustomTool {
                tool_name: name.to_string(),
                arguments: arguments_json.to_string(),
            },
        };

        serde_json::to_string(&intention)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Serialization error: {e}")))
    }

    /// List all registered tool names.
    fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Get a single tool definition by name, or None.
    fn get(&self, py: Python<'_>, name: &str) -> PyResult<Option<PyObject>> {
        match self.tools.get(name) {
            Some(tool) => {
                let dict = PyDict::new_bound(py);
                dict.set_item("name", &tool.definition.name)?;
                dict.set_item("description", &tool.definition.description)?;
                let params_dict = json_value_to_pydict(py, &tool.definition.parameters)?;
                dict.set_item("parameters", params_dict)?;
                Ok(Some(dict.into_py(py)))
            }
            None => Ok(None),
        }
    }

    fn __len__(&self) -> usize {
        self.tools.len()
    }

    fn __contains__(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    fn __repr__(&self) -> String {
        let names: Vec<&str> = self.tools.keys().map(|s| s.as_str()).collect();
        format!("ToolRegistry(tools={names:?})")
    }
}

impl PyToolRegistry {
    /// Introspect a Python callable and register it.
    fn register_from_callable(
        &mut self,
        py: Python<'_>,
        func: &PyObject,
        name_override: Option<String>,
        description_override: Option<String>,
        intention: Option<String>,
    ) -> PyResult<()> {
        let (inferred_name, inferred_desc, schema, is_async) = introspect_function(py, func)?;

        let name = name_override.unwrap_or(inferred_name);
        let description = description_override.unwrap_or(inferred_desc);

        self.tools.insert(
            name.clone(),
            RegisteredTool {
                definition: ToolDefinition {
                    name,
                    description,
                    parameters: schema,
                },
                callable: func.clone_ref(py),
                is_async,
                intention,
            },
        );
        Ok(())
    }
}

/// Internal decorator returned by `ToolRegistry.tool(description=..., ...)`.
/// Captures registration options and applies them when called with a function.
#[pyclass(name = "_ToolDecorator")]
struct ToolDecorator {
    registry: Py<PyToolRegistry>,
    description: Option<String>,
    name: Option<String>,
    intention: Option<String>,
}

#[pymethods]
impl ToolDecorator {
    fn __call__(&self, py: Python<'_>, func: PyObject) -> PyResult<PyObject> {
        let mut registry = self.registry.borrow_mut(py);
        registry.register_from_callable(
            py,
            &func,
            self.name.clone(),
            self.description.clone(),
            self.intention.clone(),
        )?;
        Ok(func)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a Python callable is an async (coroutine) function.
fn check_is_async(py: Python<'_>, func: &PyObject) -> PyResult<bool> {
    let inspect = py.import_bound("inspect")?;
    inspect
        .call_method1("iscoroutinefunction", (func.bind(py),))?
        .extract()
}

/// Introspect a Python function to extract name, docstring, JSON Schema, and async flag.
pub(crate) fn introspect_function(
    py: Python<'_>,
    func: &PyObject,
) -> PyResult<(String, String, serde_json::Value, bool)> {
    let inspect = py.import_bound("inspect")?;

    // Name
    let name: String = func.getattr(py, "__name__")?.extract(py)?;

    // Docstring — use first line, default to empty
    let description: String = func
        .getattr(py, "__doc__")
        .ok()
        .and_then(|d| {
            if d.is_none(py) {
                None
            } else {
                d.extract::<String>(py).ok()
            }
        })
        .map(|s| s.lines().next().unwrap_or("").trim().to_string())
        .unwrap_or_default();

    // Async?
    let is_async: bool = inspect
        .call_method1("iscoroutinefunction", (func.bind(py),))?
        .extract()?;

    // Signature + type hints → JSON Schema
    let sig = inspect.call_method1("signature", (func.bind(py),))?;
    let params = sig.getattr("parameters")?;
    let params_dict = params.call_method0("items")?;

    let typing = py.import_bound("typing")?;
    let get_type_hints = typing.getattr("get_type_hints")?;
    let hints = get_type_hints.call1((func.bind(py),))?;
    let hints_dict = hints.downcast::<PyDict>()?;

    let empty_sentinel = inspect.getattr("Parameter")?.getattr("empty")?;

    let mut properties = serde_json::Map::new();
    let mut required: Vec<serde_json::Value> = Vec::new();

    for item in params_dict.iter()? {
        let item = item?;
        let tuple = item.downcast::<pyo3::types::PyTuple>()?;
        let param_name: String = tuple.get_item(0)?.extract()?;
        let param_obj = tuple.get_item(1)?;

        // Skip 'return' annotation
        if param_name == "return" {
            continue;
        }

        // Get JSON Schema type from hint
        let type_schema = match hints_dict.get_item(&param_name)? {
            Some(hint) => python_type_to_json_schema(py, &hint)?,
            None => serde_json::json!({"type": "string"}),
        };

        properties.insert(param_name.clone(), type_schema);

        // Required if no default value
        let default = param_obj.getattr("default")?;
        if default.is(&empty_sentinel) {
            required.push(serde_json::Value::String(param_name));
        }
    }

    let schema = serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    });

    Ok((name, description, schema, is_async))
}

/// Map a Python type annotation to a JSON Schema type object.
fn python_type_to_json_schema(
    py: Python<'_>,
    type_obj: &Bound<'_, PyAny>,
) -> PyResult<serde_json::Value> {
    let builtins = py.import_bound("builtins")?;

    // Check primitive types
    if type_obj.is(&builtins.getattr("str")?) {
        return Ok(serde_json::json!({"type": "string"}));
    }
    if type_obj.is(&builtins.getattr("int")?) {
        return Ok(serde_json::json!({"type": "integer"}));
    }
    if type_obj.is(&builtins.getattr("float")?) {
        return Ok(serde_json::json!({"type": "number"}));
    }
    if type_obj.is(&builtins.getattr("bool")?) {
        return Ok(serde_json::json!({"type": "boolean"}));
    }

    // Check typing generics (Optional, List, Dict)
    let typing = py.import_bound("typing")?;
    let get_origin = typing.getattr("get_origin")?;
    let get_args = typing.getattr("get_args")?;

    if let Ok(origin) = get_origin.call1((type_obj,)) {
        if !origin.is_none() {
            // Optional[X] = Union[X, None]
            let union_type = typing.getattr("Union")?;
            if origin.is(&union_type) {
                if let Ok(args) = get_args.call1((type_obj,)) {
                    let args_tuple = args.downcast::<pyo3::types::PyTuple>()?;
                    // Find the non-None type
                    for i in 0..args_tuple.len() {
                        let arg = args_tuple.get_item(i)?;
                        let none_type = py.None().bind(py).get_type().into_any();
                        if !arg.is(&none_type) {
                            return python_type_to_json_schema(py, &arg);
                        }
                    }
                }
            }

            // list[X]
            if origin.is(&builtins.getattr("list")?) {
                let inner = if let Ok(args) = get_args.call1((type_obj,)) {
                    let args_tuple = args.downcast::<pyo3::types::PyTuple>()?;
                    if args_tuple.len() > 0 {
                        python_type_to_json_schema(py, &args_tuple.get_item(0)?)?
                    } else {
                        serde_json::json!({})
                    }
                } else {
                    serde_json::json!({})
                };
                return Ok(serde_json::json!({"type": "array", "items": inner}));
            }

            // dict[K, V]
            if origin.is(&builtins.getattr("dict")?) {
                return Ok(serde_json::json!({"type": "object"}));
            }
        }
    }

    // Bare list / dict without generics
    if type_obj.is(&builtins.getattr("list")?) {
        return Ok(serde_json::json!({"type": "array"}));
    }
    if type_obj.is(&builtins.getattr("dict")?) {
        return Ok(serde_json::json!({"type": "object"}));
    }

    // Fallback: treat as string
    Ok(serde_json::json!({"type": "string"}))
}

/// Run a Python coroutine, handling the case where an event loop is already running.
#[allow(dead_code)]
pub(crate) fn run_coroutine_pub(py: Python<'_>, coro: &PyObject) -> PyResult<PyObject> {
    run_coroutine(py, coro)
}

/// Convert a Python dict to JSON via json.dumps. Public so the auto-discovery
/// loader can hand a serialised result back to the catalog.
#[allow(dead_code)]
pub(crate) fn json_value_to_pydict_pub<'py>(
    py: Python<'py>,
    value: &serde_json::Value,
) -> PyResult<Bound<'py, PyDict>> {
    json_value_to_pydict(py, value)
}


///
/// Tries `asyncio.run()` first. If that fails because a loop is already running,
/// creates a new thread with its own event loop to execute the coroutine.
fn run_coroutine(py: Python<'_>, coro: &PyObject) -> PyResult<PyObject> {
    let asyncio = py.import_bound("asyncio")?;

    // Try asyncio.run() first — works when no loop is running
    match asyncio.call_method1("run", (coro.bind(py),)) {
        Ok(result) => Ok(result.unbind()),
        Err(err) => {
            // Check if this is "cannot be called from a running event loop"
            let is_loop_running = err
                .value_bound(py)
                .str()
                .map(|s| s.to_string().contains("running event loop"))
                .unwrap_or(false);

            if !is_loop_running {
                return Err(err);
            }

            // Fallback: run in a new thread with its own event loop
            let threading = py.import_bound("threading")?;
            let result_holder: PyObject = py.eval_bound("[None, None]", None, None)?.unbind();

            let code = r#"
def _zymi_run_coro(coro, holder):
    import asyncio
    loop = asyncio.new_event_loop()
    try:
        holder[0] = loop.run_until_complete(coro)
    except Exception as e:
        holder[1] = e
    finally:
        loop.close()
"#;
            py.run_bound(code, None, None)?;
            let run_fn = py.eval_bound("_zymi_run_coro", None, None)?;

            let kwargs = pyo3::types::PyDict::new_bound(py);
            kwargs.set_item("target", &run_fn)?;
            kwargs.set_item("args", (coro.bind(py), result_holder.bind(py)))?;
            let thread = threading.getattr("Thread")?.call((), Some(&kwargs))?;
            thread.call_method0("start")?;

            // Release GIL while waiting for the thread
            py.allow_threads(|| {
                std::thread::sleep(std::time::Duration::from_millis(1));
            });
            thread.call_method0("join")?;

            // Check for exception
            let holder_list = result_holder.bind(py);
            let exc = holder_list.get_item(1)?;
            if !exc.is_none() {
                return Err(PyErr::from_value_bound(exc.into_any()));
            }

            Ok(holder_list.get_item(0)?.unbind())
        }
    }
}

/// Convert a Python dict to a `serde_json::Value` via json.dumps.
fn dict_to_json_value(dict: &Bound<'_, PyDict>) -> PyResult<serde_json::Value> {
    let py = dict.py();
    let json_mod = py.import_bound("json")?;
    let json_str: String = json_mod.call_method1("dumps", (dict,))?.extract()?;
    serde_json::from_str(&json_str)
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyValueError, _>(format!("Invalid JSON: {e}")))
}

/// Convert a `serde_json::Value` (expected to be an object) to a Python dict.
fn json_value_to_pydict<'py>(
    py: Python<'py>,
    value: &serde_json::Value,
) -> PyResult<Bound<'py, PyDict>> {
    let json_str = serde_json::to_string(value).map_err(|e| {
        PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Serialization error: {e}"))
    })?;
    let json_mod = py.import_bound("json")?;
    let obj = json_mod.call_method1("loads", (PyString::new_bound(py, &json_str),))?;
    obj.downcast_into::<PyDict>()
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyTypeError, _>(format!("Expected dict: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_tool_creates_intention_json() {
        // Verify intention mapping without Python interpreter
        let intention = Intention::CallCustomTool {
            tool_name: "search".into(),
            arguments: r#"{"query":"rust"}"#.into(),
        };
        let json = serde_json::to_string(&intention).unwrap();
        assert!(json.contains("call_custom_tool"));
        assert!(json.contains("search"));
    }

    #[test]
    fn builtin_intention_mapping() {
        let intention = Intention::WebSearch {
            query: "rust async".into(),
        };
        let json = serde_json::to_string(&intention).unwrap();
        let deserialized: Intention = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.tag(), "web_search");
    }

    #[test]
    fn tool_definition_roundtrip() {
        let def = ToolDefinition {
            name: "search".into(),
            description: "Search the web".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                },
                "required": ["query"]
            }),
        };
        let json = serde_json::to_string(&def).unwrap();
        let restored: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "search");
        assert_eq!(
            restored.parameters["properties"]["query"]["type"],
            "string"
        );
    }
}
