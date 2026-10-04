//! Named LLM providers (ADR-0044).
//!
//! `$ZYMI_HOME/providers.yml` maps a name to the fields of [`LlmConfig`], so
//! one endpoint is declared once per machine instead of once per project:
//!
//! ```yaml
//! neuraldeep:
//!   provider: openai
//!   base_url: https://api.neuraldeep.ru/v1
//!   api_key: ${env.NEURALDEEP_API_KEY}
//!   model: qwen3.8-27b
//! ```
//!
//! `project.yml`'s `llm:` then accepts `llm: neuraldeep` or
//! `llm: { use: neuraldeep, model: other }` besides the inline mapping. The
//! reference is resolved while the project is deserialised, so the runtime
//! only ever sees a plain [`LlmConfig`].

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer};
use serde_yml::{Mapping, Value};

use super::project::LlmConfig;
use super::template::resolve_env_templates;

/// Path of the machine-wide providers file, if a home dir is known.
pub fn providers_path() -> Option<PathBuf> {
    super::home::zymi_home().map(|h| h.join("providers.yml"))
}

/// Accepted shapes of `project.yml`'s `llm:` — for `zymi schema` only; the
/// actual parsing goes through [`deserialize_llm`] to keep serde's precise
/// error messages for the inline form.
#[derive(JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
pub(crate) enum LlmSetting {
    /// Name of a provider in `$ZYMI_HOME/providers.yml`.
    Named(String),
    /// A named provider with field overrides, e.g. a different `model`.
    Ref(LlmRef),
    /// The full provider config, inline.
    Inline(LlmConfig),
}

#[derive(JsonSchema)]
#[allow(dead_code)]
pub(crate) struct LlmRef {
    /// Name of a provider in `$ZYMI_HOME/providers.yml`.
    #[serde(rename = "use")]
    use_: String,
    provider: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
}

/// `deserialize_with` for `ProjectConfig::llm`: shape only. The inline form
/// is parsed here (keeping serde's precise field errors); a named reference
/// yields `None` and is resolved by [`resolve_project_llm`] after the whole
/// file parsed — a provider that can't be resolved must not take the whole
/// project down with it (ADR-0044).
pub(crate) fn deserialize_llm<'de, D>(deserializer: D) -> Result<Option<LlmConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    match Option::<Value>::deserialize(deserializer)? {
        None => Ok(None),
        Some(value) => match parse_llm_value(value)? {
            LlmValue::Inline(map) => LlmConfig::deserialize(Value::Mapping(map))
                .map(Some)
                .map_err(D::Error::custom),
            LlmValue::Named { .. } => Ok(None),
        },
    }
}

enum LlmValue {
    Inline(Mapping),
    Named { name: String, overrides: Mapping },
}

fn parse_llm_value<E: serde::de::Error>(value: Value) -> Result<LlmValue, E> {
    match value {
        Value::String(name) => Ok(LlmValue::Named { name, overrides: Mapping::new() }),
        Value::Mapping(mut map) => match map.remove("use") {
            Some(Value::String(name)) => Ok(LlmValue::Named { name, overrides: map }),
            Some(_) => Err(E::custom("`llm.use` must be a provider name (a string)")),
            None => Ok(LlmValue::Inline(map)),
        },
        _ => Err(E::custom("`llm` must be a provider name or a mapping")),
    }
}

/// Second half of `llm:` loading: if the (env-resolved) project YAML names a
/// provider, resolve it against `$ZYMI_HOME/providers.yml`. `Ok(None)` when
/// `llm:` is absent or inline (already handled by [`deserialize_llm`]);
/// `Err` carries a message the runtime surfaces only if an agent step
/// actually needs the model.
pub(crate) fn resolve_project_llm(project_yaml: &str) -> Result<Option<LlmConfig>, String> {
    #[derive(Deserialize)]
    struct LlmOnly {
        #[serde(default)]
        llm: Option<Value>,
    }
    let Ok(LlmOnly { llm: Some(value) }) = serde_yml::from_str::<LlmOnly>(project_yaml) else {
        return Ok(None);
    };
    match parse_llm_value::<serde_yml::Error>(value).map_err(|e| e.to_string())? {
        LlmValue::Inline(_) => Ok(None),
        LlmValue::Named { name, overrides } => {
            let path = providers_path().ok_or_else(|| {
                format!("`llm: {name}` names a provider, but no home dir is known: set ZYMI_HOME or HOME")
            })?;
            resolve_named(&name, overrides, &path).map(Some)
        }
    }
}

/// Look `name` up in the providers file at `path`, apply `overrides` on top
/// and build the [`LlmConfig`]. `${env.*}` is resolved only inside the
/// selected entry, so an unset key for an unused provider is not an error.
pub(crate) fn resolve_named(
    name: &str,
    overrides: Mapping,
    path: &Path,
) -> Result<LlmConfig, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "`llm: {name}` names a provider, but {} could not be read ({e}). \
             Define it there, or put the full provider config inline under `llm:`",
            path.display()
        )
    })?;
    let mut providers: Mapping = serde_yml::from_str::<Option<Mapping>>(&raw)
        .map_err(|e| format!("invalid YAML in {}: {e}", path.display()))?
        .unwrap_or_default();

    let Some(entry) = providers.remove(name) else {
        let mut known: Vec<&str> = providers.keys().filter_map(Value::as_str).collect();
        known.sort_unstable();
        return Err(format!(
            "unknown provider `{name}` — {} defines: {}",
            path.display(),
            if known.is_empty() { "(none)".to_string() } else { known.join(", ") }
        ));
    };
    let Value::Mapping(mut fields) = entry else {
        return Err(format!(
            "provider `{name}` in {} must be a mapping of provider/base_url/model/api_key",
            path.display()
        ));
    };

    for value in fields.values_mut() {
        if let Value::String(s) = value {
            *s = resolve_env_templates(s, path).map_err(|e| format!("provider `{name}`: {e}"))?;
        }
    }
    for (key, value) in overrides {
        fields.insert(key, value);
    }

    LlmConfig::deserialize(Value::Mapping(fields))
        .map_err(|e| format!("provider `{name}` in {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn providers_file(dir: &TempDir, yaml: &str) -> PathBuf {
        let path = dir.path().join("providers.yml");
        std::fs::write(&path, yaml).unwrap();
        path
    }

    const PROVIDERS: &str = "\
neuraldeep:
  provider: openai
  base_url: https://api.neuraldeep.ru/v1
  api_key: ${env.ZYMI_TEST_PROVIDERS_KEY}
  model: qwen3.8-27b
broken:
  provider: openai
  model: x
  api_key: ${env.ZYMI_TEST_PROVIDERS_UNSET}
";

    #[test]
    fn named_provider_resolves_env_and_overrides() {
        let dir = TempDir::new().unwrap();
        let path = providers_file(&dir, PROVIDERS);
        unsafe { std::env::set_var("ZYMI_TEST_PROVIDERS_KEY", "sk-nd") };

        let plain = resolve_named("neuraldeep", Mapping::new(), &path).unwrap();
        assert_eq!(plain.base_url.as_deref(), Some("https://api.neuraldeep.ru/v1"));
        assert_eq!(plain.api_key.as_deref(), Some("sk-nd"));
        assert_eq!(plain.model, "qwen3.8-27b");

        let mut overrides = Mapping::new();
        overrides.insert("model".into(), "qwen3-coder".into());
        let custom = resolve_named("neuraldeep", overrides, &path).unwrap();
        assert_eq!(custom.model, "qwen3-coder");
        assert_eq!(custom.api_key.as_deref(), Some("sk-nd"));
        // An unused provider's unset env var (`broken`) did not get in the way.
    }

    #[test]
    fn unknown_provider_lists_known_ones() {
        let dir = TempDir::new().unwrap();
        let path = providers_file(&dir, PROVIDERS);
        let err = resolve_named("nope", Mapping::new(), &path).unwrap_err();
        assert!(err.contains("unknown provider `nope`"), "{err}");
        assert!(err.contains("broken, neuraldeep"), "{err}");
    }

    #[test]
    fn missing_providers_file_says_where_to_define_it() {
        let dir = TempDir::new().unwrap();
        let err = resolve_named("x", Mapping::new(), &dir.path().join("providers.yml"))
            .unwrap_err();
        assert!(err.contains("providers.yml could not be read"), "{err}");
    }

    #[test]
    fn inline_llm_keeps_serde_field_errors() {
        #[derive(Deserialize)]
        struct P {
            #[serde(default, deserialize_with = "deserialize_llm")]
            llm: Option<LlmConfig>,
        }
        let p: P = serde_yml::from_str("llm:\n  provider: openai\n  model: gpt-4o\n").unwrap();
        assert_eq!(p.llm.unwrap().model, "gpt-4o");
        let p: P = serde_yml::from_str("{}").unwrap();
        assert!(p.llm.is_none());
        // A named reference is not the deserializer's business.
        let p: P = serde_yml::from_str("llm: somewhere\n").unwrap();
        assert!(p.llm.is_none());
        let err = serde_yml::from_str::<P>("llm:\n  provider: openai\n").err().unwrap();
        assert!(err.to_string().contains("missing field `model`"), "{err}");
    }
}
