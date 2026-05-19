//! `zymi fetch` — wraps `uv sync` to build the project's own `.venv`
//! from its `pyproject.toml` (ADR-0032).
//!
//! Rationale lives in adr/0032-install-ux-fetch.md. The short version:
//! the global `zymi` (installed via `uv tool install zymi-core`) detects
//! `./.venv/bin/python` at startup and re-execs into it for pipeline-run
//! commands. This subcommand is what *makes* that `.venv` exist.

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn exec(root: PathBuf) -> Result<(), String> {
    let pyproject = root.join("pyproject.toml");
    if !pyproject.exists() {
        return Err(format!(
            "no pyproject.toml in {} — run `zymi init` first, or cd into a zymi project",
            root.display()
        ));
    }

    let uv = locate_uv()?;

    println!("zymi fetch: syncing project venv via `uv sync` ({})", root.display());

    let status = Command::new(&uv)
        .arg("sync")
        .current_dir(&root)
        .status()
        .map_err(|e| format!("failed to spawn `uv sync`: {e}"))?;

    if !status.success() {
        return Err(match status.code() {
            Some(code) => format!("`uv sync` exited with status {code}"),
            None => "`uv sync` was terminated by signal".into(),
        });
    }

    let venv_bin = if cfg!(windows) {
        root.join(".venv").join("Scripts")
    } else {
        root.join(".venv").join("bin")
    };
    println!();
    println!("Ready. Project .venv is at {}", venv_bin.parent().unwrap().display());
    println!("Next:  zymi run <pipeline>   # or `zymi serve <pipeline>`");

    Ok(())
}

/// Locate the `uv` binary on PATH. Returns a descriptive error pointing
/// the user at the install instructions when missing — `uv` is now a
/// hard dependency of the recommended install path.
fn locate_uv() -> Result<PathBuf, String> {
    which("uv").ok_or_else(|| {
        "`uv` not found on PATH. zymi fetch wraps `uv sync` — install uv first:\n  \
         curl -LsSf https://astral.sh/uv/install.sh | sh   # macOS / Linux\n  \
         irm https://astral.sh/uv/install.ps1 | iex        # Windows\n\
         See https://docs.astral.sh/uv/getting-started/installation/"
            .into()
    })
}

/// Minimal cross-platform PATH lookup. We avoid pulling in the `which`
/// crate just for this — it's a one-screen helper.
fn which(name: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path_env) {
        for ext in exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}
