use std::collections::HashMap;
use std::path::Path;

use crate::commands::RunPipeline;
use crate::config::load_project_dir;
use crate::handlers::run_pipeline;
use crate::runtime::Runtime;

pub fn exec(
    pipeline: &str,
    raw_inputs: &[String],
    approval_mode: &str,
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

    let workspace =
        load_project_dir(root).map_err(|e| format!("failed to load project: {e}"))?;

    let pipeline_config = workspace.pipelines.get(pipeline).ok_or_else(|| {
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

    let rt = super::runtime();
    let _guard = rt.enter();
    let approval = super::prepare_approval(approval_mode)?;

    let mut builder = Runtime::builder(workspace, root.to_path_buf());
    if let Some(name) = approval.channel.as_deref() {
        builder = builder.with_approval_channel(name);
    }
    let runtime = rt.block_on(builder.build_async())?;

    let approval_channels = rt.block_on(super::start_approval_channels(
        approval_mode,
        std::sync::Arc::clone(runtime.bus()),
        callback_url,
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
