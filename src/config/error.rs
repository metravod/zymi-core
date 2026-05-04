// miette's Diagnostic derive macro generates code that triggers unused_assignments warnings.
#![allow(unused_assignments)]

use std::path::PathBuf;
use std::sync::Arc;

use miette::{Diagnostic, NamedSource, SourceSpan};
use thiserror::Error;

#[derive(Debug, Diagnostic, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {path}")]
    #[diagnostic(code(zymi::config::io))]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid YAML in {path}")]
    #[diagnostic(code(zymi::config::parse))]
    Parse {
        path: PathBuf,
        #[source_code]
        src: Arc<NamedSource<String>>,
        #[label("here")]
        span: SourceSpan,
        #[help]
        detail: String,
    },

    #[error("validation error in {path}: {message}")]
    #[diagnostic(code(zymi::config::validate), help("{help}"))]
    Validation {
        message: String,
        help: String,
        path: PathBuf,
    },

    #[error("unresolved template variable `${{{var}}}` in {path}")]
    #[diagnostic(
        code(zymi::config::template),
        help("define it in project.yml `variables`, set the env var, or use ${{inputs.*}} for pipeline runtime vars")
    )]
    UnresolvedVariable { var: String, path: PathBuf },

    #[error("cycle detected in pipeline `{pipeline}` steps: {}", cycle.join(" -> "))]
    #[diagnostic(
        code(zymi::config::cycle),
        help("remove or re-route one of the depends_on edges to break the cycle")
    )]
    CyclicDependency { pipeline: String, cycle: Vec<String> },

    #[error("duplicate {kind} name `{name}`: defined in both {first} and {second}")]
    #[diagnostic(
        code(zymi::config::duplicate),
        help("rename one of the {kind}s to have a unique name")
    )]
    DuplicateName {
        kind: String,
        name: String,
        first: PathBuf,
        second: PathBuf,
    },

    #[error("unsupported store URL `{url}` in project.yml")]
    #[diagnostic(
        code(zymi::config::store),
        help(
            "set `store:` to one of: `sqlite` (default), `sqlite:<path>`, or `postgres://...`"
        )
    )]
    UnsupportedStoreUrl { url: String },
}

/// Build a [`ConfigError::Parse`] from a serde_yml error and the raw source.
pub(crate) fn parse_error(path: &std::path::Path, raw: &str, err: serde_yml::Error) -> ConfigError {
    let location = err.location();
    let (offset, len) = match location {
        Some(loc) => {
            // Convert line/column to byte offset.
            let mut byte_off = 0usize;
            for (i, line) in raw.lines().enumerate() {
                if i + 1 == loc.line() {
                    byte_off += loc.column().saturating_sub(1);
                    break;
                }
                byte_off += line.len() + 1; // +1 for newline
            }
            (byte_off.min(raw.len()), 1)
        }
        None => (0, raw.len().min(1)),
    };

    ConfigError::Parse {
        path: path.to_path_buf(),
        src: Arc::new(NamedSource::new(path.display().to_string(), raw.to_owned())),
        span: SourceSpan::new(offset.into(), len),
        detail: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_has_span() {
        let bad_yaml = "name: test\nbad:\n  - [invalid";
        let err = serde_yml::from_str::<serde_yml::Value>(bad_yaml).unwrap_err();
        let config_err = parse_error(std::path::Path::new("test.yml"), bad_yaml, err);
        assert!(matches!(config_err, ConfigError::Parse { .. }));
    }

    #[test]
    fn unresolved_variable_display() {
        let err = ConfigError::UnresolvedVariable {
            var: "env.MISSING".into(),
            path: PathBuf::from("project.yml"),
        };
        let msg = err.to_string();
        assert!(msg.contains("env.MISSING"));
        assert!(msg.contains("project.yml"));
    }
}
