//! The personal home project (ADR-0044).
//!
//! `$ZYMI_HOME`, defaulting to `~/.zymi`, is an ordinary zymi project that
//! project-scoped CLI commands fall back to when the cwd is not a project.
//! It also holds machine-wide config shared by every project, such as
//! `providers.yml`.

use std::path::PathBuf;

/// Location of the home project. `None` only when neither `ZYMI_HOME` nor a
/// user home directory (`HOME`, or `USERPROFILE` on Windows) is set.
pub fn zymi_home() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("ZYMI_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    let user_home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))?;
    Some(PathBuf::from(user_home).join(".zymi"))
}
