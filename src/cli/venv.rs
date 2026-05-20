//! Per-project `.venv` re-exec (ADR-0032).
//!
//! When `zymi` is installed globally (`uv tool install zymi-core`) and
//! the user is sitting in a project where `zymi fetch` has built a
//! `./.venv/`, we want pipeline-run commands (`run`, `serve`, `resume`)
//! to execute against the *project's* Python deps — not the global
//! tool env's. We achieve that by re-execing the project's own
//! `./.venv/bin/zymi` console_script, which lives inside the project's
//! venv and therefore links against that venv's Python interpreter.
//!
//! The `ZYMI_VENV_REEXEC` env var marks "we have already hopped" so
//! the child invocation does not loop.

use std::path::{Path, PathBuf};

use super::Cli;

/// Subcommands that execute user `@tool` code and therefore benefit
/// from running inside the project's `.venv`. Returns the configured
/// `--dir` (if any) so the caller can resolve the project root.
fn pipeline_run_dir(cli: &Cli) -> Option<Option<&Path>> {
    use super::Command;
    match &cli.command {
        Command::Run { dir, .. }
        | Command::Serve { dir, .. }
        | Command::Resume { dir, .. } => Some(dir.as_deref()),
        _ => None,
    }
}

/// Entry point — call once at process start, after clap parsing, before
/// any other work. `forward_args` is the user-intent argv *without*
/// argv[0]: when invoked through the Python entry-point (the wheel's
/// `zymi = zymi._cli:main` console script) it must come from the argv
/// that `cli_main` received, not from `std::env::args()` — the latter
/// includes the Python interpreter path inserted by the shebang, which
/// would leak into the child as a bogus subcommand. Returns normally
/// if no re-exec was performed; otherwise replaces (Unix) or wraps
/// (Windows) the current process and exits.
pub fn maybe_reexec(cli: &Cli, forward_args: &[String]) {
    if cli.no_venv {
        return;
    }
    if std::env::var_os("ZYMI_VENV_REEXEC").is_some() {
        // We're already the child of a previous re-exec — don't loop.
        return;
    }
    let Some(dir_opt) = pipeline_run_dir(cli) else {
        return;
    };
    let root = match dir_opt {
        Some(d) => d.to_path_buf(),
        None => match std::env::current_dir() {
            Ok(c) => c,
            Err(_) => return,
        },
    };

    let target = venv_zymi_path(&root);
    if !target.is_file() {
        return;
    }

    // Don't re-exec into ourselves — contributor scenario where the
    // current process *is* the project's .venv/bin/zymi.
    if let (Ok(current), Ok(target_canon)) =
        (std::env::current_exe(), target.canonicalize())
    {
        if let Ok(current_canon) = current.canonicalize() {
            if current_canon == target_canon {
                return;
            }
        }
    }

    reexec(&target, forward_args);
}

fn venv_zymi_path(root: &Path) -> PathBuf {
    if cfg!(windows) {
        root.join(".venv").join("Scripts").join("zymi.exe")
    } else {
        root.join(".venv").join("bin").join("zymi")
    }
}

#[cfg(unix)]
fn reexec(target: &Path, args: &[String]) -> ! {
    use std::os::unix::process::CommandExt;

    let target_str = target.to_string_lossy().to_string();

    let err = std::process::Command::new(target)
        .args(args)
        .env("ZYMI_VENV_REEXEC", &target_str)
        .exec();
    // `exec` only returns on failure.
    eprintln!(
        "error: failed to re-exec into project venv ({}): {err}\n\
         hint: rerun with `--no-venv` to skip the venv hop",
        target.display()
    );
    std::process::exit(1);
}

#[cfg(not(unix))]
fn reexec(target: &Path, args: &[String]) -> ! {
    let target_str = target.to_string_lossy().to_string();

    let status = std::process::Command::new(target)
        .args(args)
        .env("ZYMI_VENV_REEXEC", &target_str)
        .status();
    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!(
                "error: failed to re-exec into project venv ({}): {e}\n\
                 hint: rerun with `--no-venv` to skip the venv hop",
                target.display()
            );
            std::process::exit(1);
        }
    }
}
