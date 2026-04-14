use std::collections::HashMap;
use std::path::Path;

use regex::Regex;

use super::error::ConfigError;

/// Resolve template variables in a raw YAML string.
///
/// Supported namespaces:
/// - `${env.VAR}` — environment variables (resolved immediately)
/// - `${project.name}`, `${project.version}` — project fields
/// - `${var}` — lookup in the provided `vars` map (project-level `variables`)
/// - `${inputs.X}` — pipeline runtime variables, **left unresolved** for later substitution
///
/// Returns the resolved string or an error for any unresolved non-`inputs.*` variable.
pub fn resolve_templates(
    raw: &str,
    vars: &HashMap<String, String>,
    path: &Path,
) -> Result<String, ConfigError> {
    let re = Regex::new(r"\$\{([^}]+)\}").expect("valid regex");
    let mut result = raw.to_owned();
    let mut unresolved: Option<String> = None;

    // Iterate in reverse so replacements don't shift offsets.
    let captures: Vec<_> = re.captures_iter(raw).collect();
    for cap in captures.iter().rev() {
        let full_match = cap.get(0).unwrap();

        // Skip placeholders inside YAML comment lines — the active implementation
        // may be a harmless shell placeholder while commented-out provider blocks
        // reference env vars the user hasn't set yet.
        if is_in_yaml_comment(raw, full_match.start()) {
            continue;
        }

        let key = cap.get(1).unwrap().as_str().trim();

        let replacement = resolve_key(key, vars);
        match replacement {
            Some(val) => {
                result.replace_range(full_match.range(), &val);
            }
            None => {
                // ${inputs.*} and ${args.*} are kept as-is for runtime resolution.
                if !key.starts_with("inputs.") && !key.starts_with("args.") {
                    unresolved = Some(key.to_owned());
                }
            }
        }
    }

    if let Some(var) = unresolved {
        return Err(ConfigError::UnresolvedVariable {
            var,
            path: path.to_path_buf(),
        });
    }

    Ok(result)
}

/// Check whether the byte offset falls on a YAML comment line.
///
/// Walks backward to the start of the line and checks if the non-whitespace
/// content begins with `#`.
fn is_in_yaml_comment(raw: &str, offset: usize) -> bool {
    let line_start = raw[..offset].rfind('\n').map_or(0, |p| p + 1);
    raw[line_start..offset].trim_start().starts_with('#')
}

/// Resolve a single template key.
fn resolve_key(key: &str, vars: &HashMap<String, String>) -> Option<String> {
    if let Some(env_name) = key.strip_prefix("env.") {
        return std::env::var(env_name).ok();
    }

    if let Some(field) = key.strip_prefix("project.") {
        return vars.get(&format!("project.{field}")).cloned();
    }

    // Pipeline inputs and tool args are left as-is for runtime resolution.
    if key.starts_with("inputs.") || key.starts_with("args.") {
        return None;
    }

    // Plain variable from the project `variables` map.
    vars.get(key).cloned()
}

/// Build the variables map from a project config's fields + variables.
pub fn build_project_vars(
    name: &str,
    version: Option<&str>,
    variables: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut vars = variables.clone();
    vars.insert("project.name".into(), name.into());
    if let Some(v) = version {
        vars.insert("project.version".into(), v.into());
    }
    vars
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_plain_variables() {
        let mut vars = HashMap::new();
        vars.insert("model".into(), "gpt-4o".into());

        let input = "model: ${model}";
        let result = resolve_templates(input, &vars, Path::new("test.yml")).unwrap();
        assert_eq!(result, "model: gpt-4o");
    }

    #[test]
    fn resolves_env_variables() {
        unsafe { std::env::set_var("ZYMI_TEST_VAR", "hello") };
        let vars = HashMap::new();

        let input = "key: ${env.ZYMI_TEST_VAR}";
        let result = resolve_templates(input, &vars, Path::new("test.yml")).unwrap();
        assert_eq!(result, "key: hello");
        unsafe { std::env::remove_var("ZYMI_TEST_VAR") };
    }

    #[test]
    fn resolves_project_fields() {
        let vars = build_project_vars("my-project", Some("0.1"), &HashMap::new());

        let input = "name: ${project.name}, ver: ${project.version}";
        let result = resolve_templates(input, &vars, Path::new("test.yml")).unwrap();
        assert_eq!(result, "name: my-project, ver: 0.1");
    }

    #[test]
    fn leaves_inputs_unresolved() {
        let vars = HashMap::new();
        let input = "task: ${inputs.query}";
        let result = resolve_templates(input, &vars, Path::new("test.yml")).unwrap();
        assert_eq!(result, "task: ${inputs.query}");
    }

    #[test]
    fn errors_on_unresolved_variable() {
        let vars = HashMap::new();
        let input = "model: ${missing_var}";
        let err = resolve_templates(input, &vars, Path::new("test.yml")).unwrap_err();
        assert!(matches!(err, ConfigError::UnresolvedVariable { var, .. } if var == "missing_var"));
    }

    #[test]
    fn multiple_variables_in_one_string() {
        let mut vars = HashMap::new();
        vars.insert("a".into(), "1".into());
        vars.insert("b".into(), "2".into());

        let input = "${a} and ${b}";
        let result = resolve_templates(input, &vars, Path::new("test.yml")).unwrap();
        assert_eq!(result, "1 and 2");
    }

    #[test]
    fn skips_placeholders_in_yaml_comments() {
        let vars = HashMap::new();
        let input = "# url: \"https://api.example.com?key=${env.MISSING_KEY}\"\nname: test";
        let result = resolve_templates(input, &vars, Path::new("test.yml")).unwrap();
        // Comment line is untouched; no error for the missing env var.
        assert_eq!(result, input);
    }

    #[test]
    fn skips_placeholders_in_indented_yaml_comments() {
        let vars = HashMap::new();
        let input = "impl:\n  #   token: \"${env.MISSING}\"\n  kind: shell";
        let result = resolve_templates(input, &vars, Path::new("test.yml")).unwrap();
        assert_eq!(result, input);
    }

    #[test]
    fn resolves_active_line_but_skips_comment() {
        let mut vars = HashMap::new();
        vars.insert("model".into(), "gpt-4o".into());
        let input = "# backup: ${env.NOT_SET}\nmodel: ${model}";
        let result = resolve_templates(input, &vars, Path::new("test.yml")).unwrap();
        assert_eq!(result, "# backup: ${env.NOT_SET}\nmodel: gpt-4o");
    }

    #[test]
    fn is_in_yaml_comment_basic() {
        assert!(is_in_yaml_comment("# ${env.KEY}", 2));
        assert!(is_in_yaml_comment("  # ${env.KEY}", 4));
        assert!(!is_in_yaml_comment("key: ${env.KEY}", 5));
        assert!(!is_in_yaml_comment("first\nkey: ${env.KEY}", 11));
        assert!(is_in_yaml_comment("first\n# ${env.KEY}", 8));
    }
}
