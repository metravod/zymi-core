use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use crate::commands::RunPipeline;
use crate::config::pipeline::PipelineInputType;
use crate::config::{load_project_dir, PipelineConfig};
use crate::handlers::run_pipeline;
use crate::runtime::Runtime;

use super::event_fmt::{DIM, RESET};

pub fn exec(
    pipeline: &str,
    raw_inputs: &[String],
    approval_mode: Option<&str>,
    callback_url: Option<&str>,
    root: impl AsRef<Path>,
) -> Result<(), String> {
    let root = root.as_ref();

    if !root.join("project.yml").exists() {
        return Err(format!(
            "no project.yml found in {}. Run `zymi init` first.",
            root.display()
        ));
    }

    let mut workspace =
        load_project_dir(root).map_err(|e| format!("failed to load project: {e}"))?;

    let pipeline_config = workspace.pipelines.get(pipeline).cloned().ok_or_else(|| {
        let available: Vec<&str> = workspace.pipelines.keys().map(|s| s.as_str()).collect();
        format!(
            "pipeline '{pipeline}' not found. Available: {}",
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        )
    })?;

    println!("Pipeline: {}", pipeline_config.name);
    if let Some(desc) = &pipeline_config.description {
        println!("  {desc}");
    }
    println!();

    let mut inputs: HashMap<String, String> = HashMap::new();
    for raw in raw_inputs {
        let (key, value) = raw
            .split_once('=')
            .ok_or_else(|| format!("invalid input '{raw}': expected KEY=VALUE format"))?;
        inputs.insert(key.to_string(), value.to_string());
    }

    // Ask for what `-i` didn't cover, but only for a human at a terminal —
    // scripts, CI and agents keep the non-interactive contract (ADR-0044).
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        prompt_missing_inputs(&pipeline_config, &mut inputs)?;
    }

    // Build the runtime for this pipeline only: whether an LLM is required
    // (ADR-0041) is then judged by the pipeline being run, so a tool-only
    // pipeline still runs from a library that also holds agent pipelines
    // and has no `llm:` (ADR-0044).
    workspace.pipelines.retain(|name, _| name == pipeline);

    let rt = super::runtime();
    let _guard = rt.enter();

    let default_channel = super::pre_resolve_approval(approval_mode, &workspace.project);
    let reasoning_default = super::pre_resolve_reasoning(&workspace.project);
    let project_for_spawn = workspace.project.clone();

    let mut builder = Runtime::builder(workspace, root.to_path_buf());
    if let Some(name) = default_channel.as_deref() {
        builder = builder.with_approval_channel(name);
    }
    if let Some(name) = reasoning_default.as_deref() {
        builder = builder.with_reasoning_channel(name);
    }
    let runtime = rt.block_on(builder.build_async())?;

    let approval_channels = rt.block_on(super::start_approval_channels(
        approval_mode,
        &project_for_spawn,
        std::sync::Arc::clone(runtime.bus()),
        callback_url,
    ))?;
    let reasoning_channels = rt.block_on(super::start_reasoning_channels(
        std::sync::Arc::clone(runtime.bus()),
    ))?;

    let cmd = RunPipeline::new(pipeline.to_string(), inputs);

    let result = rt.block_on(run_pipeline::handle(&runtime, cmd));

    // Always shut down MCP subprocesses before returning — this publishes
    // McpServerDisconnected events for the TUI and lets `kill_on_drop`
    // reap predictably. Declarative connectors / outputs get the same
    // best-effort cancellation.
    rt.block_on(runtime.shutdown_connectors());
    rt.block_on(runtime.shutdown_mcp());
    for handle in approval_channels {
        rt.block_on(handle.shutdown());
    }
    for handle in reasoning_channels {
        rt.block_on(handle.shutdown());
    }

    let result = result?;

    println!("---");
    if result.success {
        println!("Pipeline completed successfully.");
    } else {
        println!("Pipeline completed with errors.");
    }

    if let Some(output) = &result.final_output {
        println!("\nFinal output:\n{output}");
    }

    if !result.success {
        return Err("pipeline had failing steps".into());
    }

    Ok(())
}

/// Prompt on stderr for each declared input not already supplied. Required
/// inputs re-ask on an empty answer; optional ones are skipped by it.
fn prompt_missing_inputs(
    cfg: &PipelineConfig,
    inputs: &mut HashMap<String, String>,
) -> Result<(), String> {
    let missing: Vec<_> = cfg
        .inputs
        .iter()
        .filter(|i| !inputs.contains_key(&i.name))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    let mut lines = std::io::stdin().lock().lines();
    for input in missing {
        if let Some(desc) = &input.description {
            eprintln!("{DIM}{}{RESET}", desc.trim());
        }
        let ty = match input.ty {
            PipelineInputType::String => String::new(),
            other => format!(" ({})", other.as_schema_str()),
        };
        let hint = if input.required { "" } else { " [Enter to skip]" };
        loop {
            eprint!("{}{ty}{hint}: ", input.name);
            std::io::stderr().flush().ok();
            let line = match lines.next() {
                Some(line) => line.map_err(|e| format!("failed to read input: {e}"))?,
                None => return Err(format!("input '{}' not provided (stdin closed)", input.name)),
            };
            let value = line.trim();
            if !value.is_empty() {
                inputs.insert(input.name.clone(), value.to_string());
                break;
            }
            if !input.required {
                break;
            }
            eprintln!("  {DIM}'{}' is required{RESET}", input.name);
        }
    }
    eprintln!();
    Ok(())
}
