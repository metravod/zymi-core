//! Generic plugin registry: `type:`-dispatched construction for YAML-driven
//! pluggable components (connectors, outputs, approvals, services, …).
//!
//! Per ADR-0020, every `project.yml` plugin section has the same shape:
//! a list of entries, each carrying a `type:` discriminator, an optional
//! `name:`, and category-specific config. The registry owns the mapping
//! `type:` → builder and performs the dispatch; the runtime behaviour of
//! each category lives behind its own trait (one trait per category — the
//! uniformity is at the registry layer, not at the trait level).
//!
//! This module ships only the construction machinery. Category traits and
//! their concrete implementations land as follow-up slices (P3 onward).

use std::collections::{HashMap, HashSet};

use serde_yml::Value as YamlValue;
use thiserror::Error;

/// Errors surfaced while walking a YAML plugin section.
#[derive(Debug, Error)]
pub enum PluginBuildError {
    #[error("plugin entry must be a YAML mapping")]
    NotAMapping,
    #[error("plugin entry is missing the 'type' discriminator")]
    MissingType,
    #[error("plugin entry 'type' must be a string")]
    TypeNotString,
    #[error("unknown plugin type '{0}'")]
    UnknownType(String),
    #[error("duplicate plugin name '{0}' in section")]
    DuplicateName(String),
    #[error("invalid config for '{name}' (type '{type_name}'): {source}")]
    InvalidConfig {
        name: String,
        type_name: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// A builder knows how to construct one concrete plugin instance of category
/// `T` from a YAML entry. Implementors typically deserialize the mapping into
/// a typed config struct and return a boxed trait object.
pub trait PluginBuilder<T: ?Sized>: Send + Sync {
    /// The `type:` discriminator this builder claims. Must be unique within
    /// a registry.
    fn type_name(&self) -> &'static str;

    /// Construct an instance. `name` is the value of the entry's `name:`
    /// field (defaulted to `type:` if absent). `entry` is the full YAML
    /// mapping — callers typically deserialize it into a typed config struct.
    fn build(
        &self,
        name: String,
        entry: YamlValue,
    ) -> Result<Box<T>, Box<dyn std::error::Error + Send + Sync>>;
}

/// Typed registry over category `T`. `T` is usually `dyn SomeTrait` (e.g.
/// `dyn InboundConnector`), but any sized target works too.
pub struct PluginRegistry<T: ?Sized> {
    builders: HashMap<&'static str, Box<dyn PluginBuilder<T>>>,
}

impl<T: ?Sized> Default for PluginRegistry<T> {
    fn default() -> Self {
        Self {
            builders: HashMap::new(),
        }
    }
}

impl<T: ?Sized> std::fmt::Debug for PluginRegistry<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("known_types", &self.builders.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl<T: ?Sized> PluginRegistry<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a builder under its [`PluginBuilder::type_name`]. Panics if
    /// the same `type:` is registered twice — registration happens at
    /// startup, and a collision is a programmer error, not a config error.
    pub fn register(&mut self, builder: Box<dyn PluginBuilder<T>>) {
        let name = builder.type_name();
        if self.builders.contains_key(name) {
            panic!("plugin type '{name}' registered twice");
        }
        self.builders.insert(name, builder);
    }

    pub fn known_types(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.builders.keys().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.builders.is_empty()
    }

    /// Build a single instance from one YAML entry. Returns `(resolved_name,
    /// instance)`. `resolved_name` is the entry's `name:` field, or the
    /// `type:` if `name:` is absent.
    pub fn build_one(&self, entry: YamlValue) -> Result<(String, Box<T>), PluginBuildError> {
        let mapping = match &entry {
            YamlValue::Mapping(m) => m,
            _ => return Err(PluginBuildError::NotAMapping),
        };

        let type_value = mapping
            .get(YamlValue::String("type".into()))
            .ok_or(PluginBuildError::MissingType)?;
        let type_name = type_value
            .as_str()
            .ok_or(PluginBuildError::TypeNotString)?
            .to_string();

        let name = mapping
            .get(YamlValue::String("name".into()))
            .and_then(|v| v.as_str())
            .unwrap_or(&type_name)
            .to_string();

        let builder = self
            .builders
            .get(type_name.as_str())
            .ok_or_else(|| PluginBuildError::UnknownType(type_name.clone()))?;

        let instance = builder
            .build(name.clone(), entry)
            .map_err(|source| PluginBuildError::InvalidConfig {
                name: name.clone(),
                type_name,
                source,
            })?;

        Ok((name, instance))
    }

    /// Build every instance from a YAML sequence. Fails fast on the first
    /// invalid entry and rejects duplicate `name:` values within the section.
    pub fn build_all(
        &self,
        entries: Vec<YamlValue>,
    ) -> Result<Vec<(String, Box<T>)>, PluginBuildError> {
        let mut out = Vec::with_capacity(entries.len());
        let mut seen: HashSet<String> = HashSet::new();
        for entry in entries {
            let (name, instance) = self.build_one(entry)?;
            if !seen.insert(name.clone()) {
                return Err(PluginBuildError::DuplicateName(name));
            }
            out.push((name, instance));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    trait Greeter: Send + Sync + std::fmt::Debug {
        fn greet(&self) -> String;
    }

    #[derive(Debug, Deserialize)]
    struct UppercaseConfig {
        subject: String,
    }

    #[derive(Debug)]
    struct UppercaseGreeter {
        subject: String,
    }

    impl Greeter for UppercaseGreeter {
        fn greet(&self) -> String {
            format!("HELLO, {}", self.subject.to_uppercase())
        }
    }

    struct UppercaseBuilder;

    impl PluginBuilder<dyn Greeter> for UppercaseBuilder {
        fn type_name(&self) -> &'static str {
            "uppercase"
        }

        fn build(
            &self,
            _name: String,
            entry: YamlValue,
        ) -> Result<Box<dyn Greeter>, Box<dyn std::error::Error + Send + Sync>> {
            let cfg: UppercaseConfig = serde_yml::from_value(entry)?;
            Ok(Box::new(UppercaseGreeter {
                subject: cfg.subject,
            }))
        }
    }

    #[derive(Debug)]
    struct EchoGreeter {
        name: String,
    }

    impl Greeter for EchoGreeter {
        fn greet(&self) -> String {
            format!("echo({})", self.name)
        }
    }

    struct EchoBuilder;

    impl PluginBuilder<dyn Greeter> for EchoBuilder {
        fn type_name(&self) -> &'static str {
            "echo"
        }

        fn build(
            &self,
            name: String,
            _entry: YamlValue,
        ) -> Result<Box<dyn Greeter>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Box::new(EchoGreeter { name }))
        }
    }

    fn registry() -> PluginRegistry<dyn Greeter> {
        let mut r: PluginRegistry<dyn Greeter> = PluginRegistry::new();
        r.register(Box::new(UppercaseBuilder));
        r.register(Box::new(EchoBuilder));
        r
    }

    fn yaml(src: &str) -> YamlValue {
        serde_yml::from_str(src).unwrap()
    }

    #[test]
    fn build_one_dispatches_on_type() {
        let r = registry();
        let (name, plugin) = r
            .build_one(yaml("type: uppercase\nname: shouter\nsubject: world"))
            .unwrap();
        assert_eq!(name, "shouter");
        assert_eq!(plugin.greet(), "HELLO, WORLD");
    }

    #[test]
    fn name_defaults_to_type_when_absent() {
        let r = registry();
        let (name, _) = r.build_one(yaml("type: echo")).unwrap();
        assert_eq!(name, "echo");
    }

    #[test]
    fn missing_type_is_error() {
        let r = registry();
        let err = r.build_one(yaml("name: oops")).unwrap_err();
        assert!(matches!(err, PluginBuildError::MissingType), "got {err:?}");
    }

    #[test]
    fn unknown_type_is_error() {
        let r = registry();
        let err = r.build_one(yaml("type: nosuch")).unwrap_err();
        match err {
            PluginBuildError::UnknownType(t) => assert_eq!(t, "nosuch"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn non_mapping_entry_is_error() {
        let r = registry();
        let err = r.build_one(yaml("- just_a_list")).unwrap_err();
        assert!(matches!(err, PluginBuildError::NotAMapping), "got {err:?}");
    }

    #[test]
    fn invalid_config_is_wrapped_with_context() {
        let r = registry();
        let err = r.build_one(yaml("type: uppercase\nname: bad")).unwrap_err();
        match err {
            PluginBuildError::InvalidConfig { name, type_name, .. } => {
                assert_eq!(name, "bad");
                assert_eq!(type_name, "uppercase");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn build_all_rejects_duplicate_names() {
        let r = registry();
        let entries = vec![
            yaml("type: echo\nname: same"),
            yaml("type: echo\nname: same"),
        ];
        match r.build_all(entries) {
            Err(PluginBuildError::DuplicateName(name)) => assert_eq!(name, "same"),
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("expected DuplicateName"),
        }
    }

    #[test]
    fn build_all_preserves_entry_order() {
        let r = registry();
        let entries = vec![
            yaml("type: echo\nname: first"),
            yaml("type: echo\nname: second"),
            yaml("type: echo\nname: third"),
        ];
        let built = match r.build_all(entries) {
            Ok(b) => b,
            Err(e) => panic!("unexpected error: {e:?}"),
        };
        let names: Vec<_> = built.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[test]
    fn known_types_reports_registered_builders() {
        let r = registry();
        let mut seen: Vec<_> = r.known_types().collect();
        seen.sort();
        assert_eq!(seen, vec!["echo", "uppercase"]);
    }

    #[test]
    #[should_panic(expected = "registered twice")]
    fn duplicate_registration_panics() {
        let mut r: PluginRegistry<dyn Greeter> = PluginRegistry::new();
        r.register(Box::new(EchoBuilder));
        r.register(Box::new(EchoBuilder));
    }
}
