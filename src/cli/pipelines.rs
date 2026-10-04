use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::pipeline::load_pipeline;

use super::event_fmt::{BOLD, DIM, RESET};

/// Sorted `*.yml` / `*.yaml` files under `<root>/pipelines/`.
fn pipeline_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let pipelines_dir = root.join("pipelines");

    if !pipelines_dir.is_dir() {
        return Err(format!(
            "no pipelines/ directory in {}. Run `zymi init` first \
             (or `zymi init --home` for a personal library at ~/.zymi).",
            root.display()
        ));
    }

    let mut entries: Vec<_> = std::fs::read_dir(&pipelines_dir)
        .map_err(|e| format!("failed to read pipelines/: {e}"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "yml" || ext == "yaml"))
        .collect();
    entries.sort();
    Ok(entries)
}

/// `zymi ls` (ADR-0044): one entry per pipeline — name, first line of the
/// description, inputs with `*` marking required ones. The human-facing
/// "what can I run" view; `zymi pipelines` stays the step-level one.
pub fn exec_ls(root: impl AsRef<Path>) -> Result<(), String> {
    let root = root.as_ref();
    let entries = pipeline_files(root)?;

    // Template vars are unresolved at this point — ${inputs.*} survives load_pipeline.
    let vars: HashMap<String, String> = HashMap::new();
    let mut loaded = Vec::new();
    for path in entries {
        match load_pipeline(&path, &vars) {
            Ok(cfg) => loaded.push(cfg),
            Err(e) => eprintln!("{DIM}[skip]{RESET} {}: failed to parse ({e})", path.display()),
        }
    }
    loaded.sort_by(|a, b| a.name.cmp(&b.name));

    if loaded.is_empty() {
        println!("No pipelines in {}.", root.display());
        return Ok(());
    }

    let noun = if loaded.len() == 1 { "pipeline" } else { "pipelines" };
    println!("{BOLD}{}{RESET} {DIM}({} {noun}){RESET}", root.display(), loaded.len());
    println!();

    let width = loaded.iter().map(|c| c.name.chars().count()).max().unwrap_or(0);
    let indent = " ".repeat(width + 4);
    for cfg in &loaded {
        let desc = cfg
            .description
            .as_deref()
            .and_then(|d| d.lines().map(str::trim).find(|l| !l.is_empty()))
            .unwrap_or("");
        println!("  {BOLD}{:<width$}{RESET}  {desc}", cfg.name);
        if !cfg.inputs.is_empty() {
            let inputs: Vec<String> = cfg
                .inputs
                .iter()
                .map(|i| {
                    if i.required {
                        format!("{}*", i.name)
                    } else {
                        i.name.clone()
                    }
                })
                .collect();
            println!("{indent}{DIM}inputs: {}{RESET}", inputs.join(", "));
        }
    }
    println!();
    println!("{DIM}zymi run <name> — missing inputs are asked interactively{RESET}");
    Ok(())
}

pub fn exec(root: impl AsRef<Path>) -> Result<(), String> {
    let root = root.as_ref();
    let pipelines_dir = root.join("pipelines");
    let entries = pipeline_files(root)?;

    if entries.is_empty() {
        println!("No pipelines found in {}.", pipelines_dir.display());
        return Ok(());
    }

    println!(
        "{BOLD}Pipelines{RESET}: {} found in {}",
        entries.len(),
        pipelines_dir.display()
    );
    println!();

    // Template vars are unresolved at this point — ${inputs.*} survives load_pipeline.
    let vars: HashMap<String, String> = HashMap::new();

    for path in entries {
        match load_pipeline(&path, &vars) {
            Ok(cfg) => {
                println!("{BOLD}{}{RESET}", cfg.name);
                if let Some(desc) = &cfg.description {
                    println!("  {DIM}description{RESET}: {desc}");
                }
                let input_names: Vec<&str> = cfg.inputs.iter().map(|i| i.name.as_str()).collect();
                println!(
                    "  {DIM}inputs{RESET}: [{}]",
                    if input_names.is_empty() {
                        "none".into()
                    } else {
                        input_names.join(", ")
                    }
                );
                println!("  {DIM}steps{RESET}: {}", cfg.steps.len());
                for step in &cfg.steps {
                    let deps = if step.depends_on.is_empty() {
                        String::new()
                    } else {
                        format!(" ← [{}]", step.depends_on.join(", "))
                    };
                    let label = match &step.kind {
                        crate::config::pipeline::PipelineStepKind::Agent { agent, .. } => {
                            agent.clone()
                        }
                        crate::config::pipeline::PipelineStepKind::Tool { tool, .. } => {
                            format!("tool:{tool}")
                        }
                        crate::config::pipeline::PipelineStepKind::Ask { channel, .. } => {
                            format!("ask:{}", channel.as_deref().unwrap_or("caller"))
                        }
                    };
                    println!(
                        "    {DIM}·{RESET} {} {DIM}({}){RESET}{deps}",
                        step.id, label
                    );
                }
                if let Some(out) = &cfg.output {
                    use crate::config::PipelineOutput;
                    let rendered = match out {
                        PipelineOutput::Step(s) => s.step.clone(),
                        PipelineOutput::AnyOf(a) => format!("any_of [{}]", a.any_of.join(", ")),
                    };
                    println!("  {DIM}output{RESET}: {rendered}");
                }
                println!();
            }
            Err(e) => {
                eprintln!(
                    "{DIM}[skip]{RESET} {}: failed to parse ({e})",
                    path.display()
                );
            }
        }
    }

    Ok(())
}
