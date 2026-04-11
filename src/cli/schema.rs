use schemars::schema_for;

use crate::config::agent::AgentConfig;
use crate::config::pipeline::PipelineConfig;
use crate::config::project::ProjectConfig;
use crate::config::tool::ToolConfig;

const KNOWN_KINDS: &[&str] = &["project", "agent", "pipeline", "tool"];

pub fn exec(kind: Option<&str>, all: bool) -> Result<(), String> {
    if all {
        let mut map = serde_json::Map::new();
        for &k in KNOWN_KINDS {
            map.insert(k.to_string(), schema_value(k)?);
        }
        let out = serde_json::to_string_pretty(&map)
            .map_err(|e| format!("serialisation error: {e}"))?;
        println!("{out}");
        return Ok(());
    }

    let kind = kind.ok_or_else(|| {
        format!("specify a config kind ({}), or use --all", KNOWN_KINDS.join(", "))
    })?;

    let schema = schema_value(kind)?;
    let out = serde_json::to_string_pretty(&schema)
        .map_err(|e| format!("serialisation error: {e}"))?;
    println!("{out}");
    Ok(())
}

fn schema_value(kind: &str) -> Result<serde_json::Value, String> {
    let schema = match kind {
        "project" => schema_for!(ProjectConfig),
        "agent" => schema_for!(AgentConfig),
        "pipeline" => schema_for!(PipelineConfig),
        "tool" => schema_for!(ToolConfig),
        other => return Err(format!(
            "unknown config kind '{other}'; expected one of: {}",
            KNOWN_KINDS.join(", "),
        )),
    };
    serde_json::to_value(schema).map_err(|e| format!("serialisation error: {e}"))
}
