use std::collections::HashMap;
use std::path::Path;

use crate::config::pipeline::load_pipeline;

use super::event_fmt::{BOLD, DIM, RESET};

pub fn exec(root: impl AsRef<Path>) -> Result<(), String> {
    let root = root.as_ref();
    let pipelines_dir = root.join("pipelines");

    if !pipelines_dir.is_dir() {
        return Err(format!(
            "no pipelines/ directory in {}. Run `zymi init` first.",
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
                    };
                    println!(
                        "    {DIM}·{RESET} {} {DIM}({}){RESET}{deps}",
                        step.id, label
                    );
                }
                if let Some(out) = &cfg.output {
                    println!("  {DIM}output{RESET}: {}", out.step);
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
