//! Auto-discovery of Python tools (ADR-0014 slice 3).
//!
//! Scans `<project_root>/tools/*.py`, imports each file via the embedded
//! Python interpreter, walks module attributes, and collects every
//! callable whose `_zymi_tool` attribute is truthy (the marker set by
//! the `@zymi.tool` decorator).
//!
//! Each discovered function becomes a [`PythonEntry`] in the
//! [`crate::runtime::ToolCatalog`] under the `python` feature. The
//! function's signature + docstring are introspected to produce the
//! LLM-facing JSON Schema, mirroring the existing
//! [`crate::python::tool::PyToolRegistry`] machinery.
//!
//! Behaviour when the `python` feature is **off** lives elsewhere — the
//! whole module is `#[cfg(feature = "python")]`-gated; the runtime falls
//! back to a "no python tools discovered" no-op.

use std::path::{Path, PathBuf};

use pyo3::prelude::*;
use pyo3::types::PyAny;

use crate::python::tool::introspect_function;

/// One auto-discovered Python tool. Owned by [`ToolCatalog`] for the
/// process lifetime once registered.
pub struct DiscoveredTool {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the function's parameters.
    pub parameters: serde_json::Value,
    /// The Python callable. `Py<PyAny>` is `Send + Sync` (unbound smart
    /// pointer) so the catalog stays `Send + Sync`.
    pub callable: Py<PyAny>,
    pub is_async: bool,
    /// Optional ESAA intention tag set via `@tool(intention="...")`.
    /// Maps the same set of names as
    /// [`crate::python::tool::PyToolRegistry::to_intention`].
    pub intention: Option<String>,
    /// Optional override set via `@tool(requires_approval=True)`.
    /// `None` means "follow the project default".
    pub requires_approval: Option<bool>,
    /// Source file path, kept for diagnostics.
    pub source: PathBuf,
}

impl std::fmt::Debug for DiscoveredTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveredTool")
            .field("name", &self.name)
            .field("is_async", &self.is_async)
            .field("intention", &self.intention)
            .field("requires_approval", &self.requires_approval)
            .field("source", &self.source)
            .finish()
    }
}

/// Scan `<project_root>/tools/*.py` for `@tool`-decorated functions.
///
/// Returns the discovered tools in alphabetical order by file name, with
/// declaration order preserved within each file. Missing `tools/`
/// directory is not an error — most projects start without Python tools.
///
/// Import failures abort discovery for that file with a contextual error;
/// other files still load. Per-file errors are returned in `errors`
/// rather than aborting the whole scan, so one broken `tools/foo.py`
/// doesn't take down a project that has a working `tools/bar.py`.
pub struct DiscoveryOutcome {
    pub tools: Vec<DiscoveredTool>,
    pub errors: Vec<(PathBuf, String)>,
}

pub fn discover_python_tools(project_root: &Path) -> DiscoveryOutcome {
    let dir = project_root.join("tools");
    if !dir.is_dir() {
        return DiscoveryOutcome {
            tools: Vec::new(),
            errors: Vec::new(),
        };
    }

    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("py"))
            .collect(),
        Err(e) => {
            return DiscoveryOutcome {
                tools: Vec::new(),
                errors: vec![(dir, e.to_string())],
            }
        }
    };
    files.sort();

    let mut tools = Vec::new();
    let mut errors = Vec::new();

    Python::with_gil(|py| {
        for path in &files {
            match discover_in_file(py, path) {
                Ok(mut found) => tools.append(&mut found),
                Err(e) => errors.push((path.clone(), e)),
            }
        }
    });

    DiscoveryOutcome { tools, errors }
}

fn discover_in_file(py: Python<'_>, path: &Path) -> Result<Vec<DiscoveredTool>, String> {
    let code = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
    let module_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("anon_zymi_tool")
        .to_string();
    let module = PyModule::from_code_bound(
        py,
        &code,
        &path.display().to_string(),
        &module_name,
    )
    .map_err(|e| format!("import: {e}"))?;

    // dir() ordering is alphabetical, but we want declaration order so
    // the discovered tools land in the same order the user wrote them —
    // less surprising in `zymi tools list` output. We use the module
    // dict's iteration order, which on CPython 3.7+ is insertion order.
    let dict = module.dict();
    let mut found = Vec::new();
    for (key, value) in dict.iter() {
        if !is_zymi_tool(&value)? {
            continue;
        }
        let attr_name: String = key
            .extract::<String>()
            .map_err(|e| format!("attribute name not a string: {e}"))?;
        let entry = build_discovered(py, &attr_name, value.unbind(), path)?;
        found.push(entry);
    }
    Ok(found)
}

fn is_zymi_tool(obj: &Bound<'_, PyAny>) -> Result<bool, String> {
    match obj.getattr("_zymi_tool") {
        Ok(v) => v.extract::<bool>().map_err(|e| e.to_string()),
        Err(_) => Ok(false),
    }
}

fn build_discovered(
    py: Python<'_>,
    attr_name: &str,
    callable: Py<PyAny>,
    source: &Path,
) -> Result<DiscoveredTool, String> {
    let (inferred_name, inferred_desc, schema, is_async) =
        introspect_function(py, &callable).map_err(|e| format!("introspect: {e}"))?;

    let bound = callable.bind(py);
    let name_override: Option<String> = optional_string(bound, "_zymi_tool_name")?;
    let desc_override: Option<String> = optional_string(bound, "_zymi_tool_description")?;
    let intention: Option<String> = optional_string(bound, "_zymi_tool_intention")?;
    let requires_approval: Option<bool> = optional_bool(bound, "_zymi_tool_requires_approval")?;

    let name = name_override.unwrap_or(inferred_name);
    if name != attr_name {
        // Either the user passed `@tool(name=...)` or the function was
        // bound under a different attribute name (rebinding). Trust the
        // explicit override but log the discrepancy at info level so
        // surprises are debuggable.
        log::info!(
            "python tool discovered as '{name}' (file attr was '{attr_name}', source {})",
            source.display()
        );
    }
    let description = desc_override.unwrap_or(inferred_desc);

    Ok(DiscoveredTool {
        name,
        description,
        parameters: schema,
        callable,
        is_async,
        intention,
        requires_approval,
        source: source.to_path_buf(),
    })
}

fn optional_string(bound: &Bound<'_, PyAny>, attr: &str) -> Result<Option<String>, String> {
    match bound.getattr(attr) {
        Ok(v) if v.is_none() => Ok(None),
        Ok(v) => v
            .extract::<String>()
            .map(Some)
            .map_err(|e| format!("{attr}: {e}")),
        Err(_) => Ok(None),
    }
}

fn optional_bool(bound: &Bound<'_, PyAny>, attr: &str) -> Result<Option<bool>, String> {
    match bound.getattr(attr) {
        Ok(v) if v.is_none() => Ok(None),
        Ok(v) => v
            .extract::<bool>()
            .map(Some)
            .map_err(|e| format!("{attr}: {e}")),
        Err(_) => Ok(None),
    }
}
