use std::path::Path;

use crate::config::load_project_dir;
use crate::events::store::{open_store_async, StoreBackend};
use crate::handlers::resume_pipeline::{self, ResumePipeline};
use crate::runtime::Runtime;

use super::event_fmt::{BOLD, DIM, GREEN, RESET, YELLOW};
use super::resolve_store_backend_for_cli;

pub fn exec(
    parent_stream: &str,
    fork_step: &str,
    dry_run: bool,
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

    let workspace =
        load_project_dir(root).map_err(|e| format!("failed to load project: {e}"))?;

    let rt = super::runtime();
    let _guard = rt.enter();

    // Dry-run path: no LLM, no approval, no execution — just plan + print.
    // Bypass Runtime::builder so projects without an `llm` block can still
    // preview a fork.
    if dry_run {
        let backend = resolve_store_backend_for_cli(root)?;
        if let StoreBackend::Sqlite { path } = &backend {
            if !path.exists() {
                return Err(format!(
                    "no event store found at {}. Cannot dry-run resume.",
                    path.display()
                ));
            }
        }
        let store = rt
            .block_on(open_store_async(backend))
            .map_err(|e| format!("failed to open event store: {e}"))?;
        let cmd = ResumePipeline {
            parent_stream_id: parent_stream.to_string(),
            fork_at_step: fork_step.to_string(),
        };
        let plan =
            rt.block_on(resume_pipeline::plan_with(store.as_ref(), &workspace, &cmd))?;
        print_plan(&plan, true);
        return Ok(());
    }

    let default_channel = super::pre_resolve_approval(approval_mode, &workspace.project);
    let project_for_spawn = workspace.project.clone();

    let mut builder = Runtime::builder(workspace, root.to_path_buf());
    if let Some(name) = default_channel.as_deref() {
        builder = builder.with_approval_channel(name);
    }
    let runtime = rt.block_on(builder.build_async())?;

    let approval_channels = rt.block_on(super::start_approval_channels(
        approval_mode,
        &project_for_spawn,
        std::sync::Arc::clone(runtime.bus()),
        callback_url,
    ))?;

    let cmd = ResumePipeline {
        parent_stream_id: parent_stream.to_string(),
        fork_at_step: fork_step.to_string(),
    };

    // Compute + print plan first so the user sees the shape of the run before
    // it starts. handle() recomputes internally — same validation, no surprise.
    let plan = rt.block_on(resume_pipeline::plan(&runtime, &cmd))?;
    print_plan(&plan, false);

    let outcome = rt.block_on(resume_pipeline::handle(&runtime, cmd))?;

    for handle in approval_channels {
        rt.block_on(handle.shutdown());
    }

    println!("---");
    println!("New stream: {}", outcome.new_stream_id);
    if outcome.result.success {
        println!("{GREEN}Pipeline completed successfully.{RESET}");
    } else {
        println!("{YELLOW}Pipeline completed with errors.{RESET}");
    }
    if let Some(out) = &outcome.result.final_output {
        println!("\nFinal output:\n{out}");
    }
    if !outcome.result.success {
        return Err("pipeline had failing steps".into());
    }
    Ok(())
}

fn print_plan(plan: &resume_pipeline::ResumePlan, dry_run: bool) {
    let header = if dry_run {
        format!("{BOLD}Resume plan{RESET} {DIM}(dry-run){RESET}")
    } else {
        format!("{BOLD}Resume plan{RESET}")
    };
    println!("{header}");
    println!(
        "  pipeline: {}    fork at: {YELLOW}{}{RESET}",
        plan.pipeline_name, plan.fork_at_step,
    );
    println!("  parent:   {DIM}{}{RESET}", plan.parent_stream_id);

    if plan.frozen_in_dag_order.is_empty() {
        println!("  {DIM}frozen:{RESET}        (none — fork step is the first step)");
    } else {
        println!(
            "  {DIM}frozen ({}):{RESET}    {}",
            plan.frozen_in_dag_order.len(),
            plan.frozen_in_dag_order.join(", "),
        );
        println!(
            "  {DIM}↳ events copied verbatim from parent; current configs NOT applied{RESET}"
        );
    }
    println!(
        "  {YELLOW}re-execute ({}):{RESET} {}",
        plan.re_executed_in_dag_order.len(),
        plan.re_executed_in_dag_order.join(", "),
    );
    if !dry_run {
        println!();
    }
}
