//! `type: cron` — timer-driven synthetic event source (ADR-0021 slice 4).
//!
//! Publishes a tick on a schedule defined by a standard cron expression.
//! Two input shapes:
//!
//! * **New (multi-input):** `cron:` + `inputs:` map → every tick fires
//!   `PipelineRequested { inputs }` with all keys set, suitable for
//!   parameterised pipelines that also need to be runnable manually with
//!   `zymi run <name> -i k=v -i k=v`.
//! * **Legacy (single-content):** `cron:` + `content:` (+ optional
//!   `pipeline_input:`) → tick fires `PipelineRequested` with one input
//!   keyed by `pipeline_input` (default `"message"`).
//!
//! Both shapes additionally publish `UserMessageReceived` so observers
//! and `zymi events` see the tick on the bus. Cron expressions accept 5
//! fields (`min hour day month weekday`) or 6 fields (`sec min hour day
//! month weekday`) for sub-minute granularity.
//!
//! No network involved — this is a clock, not an integration.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Local;
use croner::Cron;
use serde::Deserialize;
use serde_yml::Value as YamlValue;
use tokio_util::sync::CancellationToken;

use crate::connectors::{ConnectorError, InboundConnector, PluginContext, PluginHandle};
use crate::events::{Event, EventKind};
use crate::plugin::PluginBuilder;
use crate::types::Message;

#[derive(Debug, Clone, Deserialize)]
struct CronConfig {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    /// Cron expression. 5 fields (`min hour day month weekday`) or 6
    /// fields (`sec min hour day month weekday`). Examples: `*/15 * * * *`
    /// (every 15 minutes), `0 9 * * *` (daily at 09:00), `*/30 * * * * *`
    /// (every 30 seconds).
    cron: String,
    /// Synthetic stream id. Defaults to the connector name.
    #[serde(default)]
    stream_id: Option<String>,
    /// Static inputs to pass to the pipeline on every tick. Keys are
    /// pipeline input names. Mutually exclusive with `content:`.
    #[serde(default)]
    inputs: HashMap<String, String>,
    /// Legacy: synthetic message content. Pre-`inputs:` form. Mutually
    /// exclusive with `inputs:`.
    #[serde(default)]
    content: Option<String>,
    /// Legacy: pipeline input key under which `content:` is placed.
    /// Defaults to `"message"`. Ignored when `inputs:` is set.
    #[serde(default)]
    pipeline_input: Option<String>,
    /// Optional: also publish `PipelineRequested` per tick.
    #[serde(default)]
    pipeline: Option<String>,
}

fn compile_schedule(expr: &str) -> Result<Cron, ConnectorError> {
    Cron::new(expr)
        .with_seconds_optional()
        .parse()
        .map_err(|e| ConnectorError::InvalidConfig(format!("cron: invalid expression '{expr}': {e}")))
}

fn validate(cfg: &CronConfig) -> Result<Cron, ConnectorError> {
    if !cfg.inputs.is_empty() && cfg.content.is_some() {
        return Err(ConnectorError::InvalidConfig(
            "cron: set either 'inputs:' or legacy 'content:', not both".into(),
        ));
    }
    if cfg.inputs.is_empty() && cfg.content.is_none() && cfg.pipeline.is_some() {
        return Err(ConnectorError::InvalidConfig(
            "cron: when 'pipeline:' is set, provide 'inputs:' (preferred) or 'content:'".into(),
        ));
    }
    compile_schedule(&cfg.cron)
}

pub struct CronBuilder;

impl PluginBuilder<dyn InboundConnector> for CronBuilder {
    fn type_name(&self) -> &'static str {
        "cron"
    }

    fn build(
        &self,
        name: String,
        entry: YamlValue,
    ) -> Result<Box<dyn InboundConnector>, Box<dyn std::error::Error + Send + Sync>> {
        let cfg: CronConfig = serde_yml::from_value(entry)?;
        let schedule = validate(&cfg)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        let _ = name;
        Ok(Box::new(CronConnector { cfg, schedule }))
    }
}

struct CronConnector {
    cfg: CronConfig,
    schedule: Cron,
}

#[async_trait]
impl InboundConnector for CronConnector {
    fn type_name(&self) -> &'static str {
        "cron"
    }

    async fn start(
        &self,
        resolved_name: String,
        ctx: PluginContext,
    ) -> Result<PluginHandle, ConnectorError> {
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task_name = resolved_name.clone();
        let task_ctx = ctx.clone();
        let cfg = self.cfg.clone();
        let schedule = self.schedule.clone();

        let join = tokio::spawn(async move {
            run_cron(task_name, cfg, schedule, task_ctx, task_shutdown).await;
        });

        Ok(PluginHandle {
            name: resolved_name,
            type_name: "cron",
            shutdown,
            join,
        })
    }
}

async fn run_cron(
    name: String,
    cfg: CronConfig,
    schedule: Cron,
    ctx: PluginContext,
    shutdown: CancellationToken,
) {
    log::info!("cron '{name}' started ({})", cfg.cron);
    loop {
        let wait = match next_wait(&schedule) {
            Some(d) => d,
            None => {
                log::warn!("cron '{name}' has no future occurrences — stopping");
                break;
            }
        };
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(wait) => {
                emit(&name, &cfg, &ctx).await;
            }
        }
    }
    log::info!("cron '{name}' stopped");
}

fn next_wait(schedule: &Cron) -> Option<Duration> {
    let now = Local::now();
    let next = schedule.find_next_occurrence(&now, false).ok()?;
    let delta = next - now;
    delta.to_std().ok().or(Some(Duration::from_millis(1)))
}

fn pipeline_inputs(cfg: &CronConfig) -> HashMap<String, String> {
    if !cfg.inputs.is_empty() {
        return cfg.inputs.clone();
    }
    if let Some(content) = cfg.content.as_deref() {
        let key = cfg.pipeline_input.as_deref().unwrap_or("message");
        let mut m = HashMap::new();
        m.insert(key.to_string(), content.to_string());
        return m;
    }
    HashMap::new()
}

fn tick_message(cfg: &CronConfig, name: &str) -> String {
    cfg.content
        .clone()
        .unwrap_or_else(|| format!("cron tick from {name}"))
}

async fn emit(name: &str, cfg: &CronConfig, ctx: &PluginContext) {
    let stream_id = cfg.stream_id.clone().unwrap_or_else(|| name.to_string());
    let correlation = uuid::Uuid::new_v4();
    let event = Event::new(
        stream_id.clone(),
        EventKind::UserMessageReceived {
            content: Message::User(tick_message(cfg, name)),
            connector: name.to_string(),
        },
        name.to_string(),
    )
    .with_correlation(correlation);

    if let Err(e) = ctx.bus.publish(event).await {
        log::warn!("cron '{name}' publish failed: {e}");
        return;
    }

    if let Some(pipeline) = cfg.pipeline.as_deref() {
        let inputs = pipeline_inputs(cfg);
        let req = Event::new(
            stream_id,
            EventKind::PipelineRequested {
                pipeline: pipeline.to_string(),
                inputs,
            },
            name.to_string(),
        )
        .with_correlation(correlation);
        if let Err(e) = ctx.bus.publish(req).await {
            log::warn!("cron '{name}' pipeline publish failed: {e}");
        }
    }
}

// Suppress "unused" on Arc — `ctx.bus` ownership matches the other
// connectors, but rustc sometimes flags the helper alias.
#[allow(dead_code)]
fn _arc_check(_b: Arc<crate::events::bus::EventBus>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::bus::EventBus;
    use crate::events::store::SqliteEventStore;

    fn yaml(src: &str) -> YamlValue {
        serde_yml::from_str(src).unwrap()
    }

    fn expect_build_err(entry: YamlValue) -> String {
        match CronBuilder.build("c".into(), entry) {
            Ok(_) => panic!("expected build failure"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn build_requires_cron_field() {
        let err = expect_build_err(yaml("type: cron\ncontent: hi"));
        assert!(err.contains("cron") || err.contains("missing"), "got {err}");
    }

    #[test]
    fn build_rejects_invalid_expression() {
        let err = expect_build_err(yaml("type: cron\ncron: \"not a cron\"\ncontent: hi"));
        assert!(err.contains("invalid expression"), "got {err}");
    }

    #[test]
    fn build_rejects_inputs_and_content_both() {
        let err = expect_build_err(yaml(
            "type: cron\ncron: \"* * * * *\"\ninputs:\n  a: \"1\"\ncontent: hi",
        ));
        assert!(err.contains("either 'inputs:' or"), "got {err}");
    }

    #[test]
    fn build_rejects_pipeline_without_inputs_or_content() {
        let err = expect_build_err(yaml(
            "type: cron\ncron: \"* * * * *\"\npipeline: digest",
        ));
        assert!(err.contains("'inputs:'"), "got {err}");
    }

    #[test]
    fn build_accepts_inputs_form() {
        let res = CronBuilder.build(
            "c".into(),
            yaml(
                r#"
type: cron
cron: "*/15 * * * *"
pipeline: sync
inputs:
  repo_url: "git@github.com:foo/bar"
  branch: "main"
"#,
            ),
        );
        assert!(res.is_ok(), "{:?}", res.err());
    }

    #[test]
    fn build_accepts_legacy_content_form() {
        let res = CronBuilder.build(
            "c".into(),
            yaml(
                r#"
type: cron
cron: "0 9 * * *"
content: "Daily digest"
pipeline: digest
"#,
            ),
        );
        assert!(res.is_ok(), "{:?}", res.err());
    }

    #[test]
    fn build_accepts_six_field_expression() {
        let res = CronBuilder.build(
            "c".into(),
            yaml("type: cron\ncron: \"*/30 * * * * *\"\ncontent: hi"),
        );
        assert!(res.is_ok(), "{:?}", res.err());
    }

    #[test]
    fn next_wait_returns_future_for_per_minute_schedule() {
        let schedule = compile_schedule("* * * * *").unwrap();
        let d = next_wait(&schedule).expect("future occurrence");
        assert!(d.as_secs() <= 60);
    }

    #[test]
    fn pipeline_inputs_prefers_inputs_map() {
        let cfg = CronConfig {
            name: None,
            cron: "* * * * *".into(),
            stream_id: None,
            inputs: HashMap::from([("a".into(), "1".into()), ("b".into(), "2".into())]),
            content: None,
            pipeline_input: None,
            pipeline: None,
        };
        let out = pipeline_inputs(&cfg);
        assert_eq!(out.get("a").map(String::as_str), Some("1"));
        assert_eq!(out.get("b").map(String::as_str), Some("2"));
    }

    #[test]
    fn pipeline_inputs_legacy_uses_pipeline_input_key() {
        let cfg = CronConfig {
            name: None,
            cron: "* * * * *".into(),
            stream_id: None,
            inputs: HashMap::new(),
            content: Some("hello".into()),
            pipeline_input: Some("query".into()),
            pipeline: None,
        };
        let out = pipeline_inputs(&cfg);
        assert_eq!(out.get("query").map(String::as_str), Some("hello"));
    }

    #[test]
    fn pipeline_inputs_legacy_default_key_is_message() {
        let cfg = CronConfig {
            name: None,
            cron: "* * * * *".into(),
            stream_id: None,
            inputs: HashMap::new(),
            content: Some("hello".into()),
            pipeline_input: None,
            pipeline: None,
        };
        let out = pipeline_inputs(&cfg);
        assert_eq!(out.get("message").map(String::as_str), Some("hello"));
    }

    #[tokio::test]
    async fn per_second_schedule_emits_user_message_received() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteEventStore::new(&dir.path().join("events.db")).unwrap());
        let bus = Arc::new(EventBus::new(store.clone()));
        let mut rx = bus.subscribe().await;

        let connector = CronBuilder
            .build(
                "tick".into(),
                yaml(
                    r#"
type: cron
cron: "* * * * * *"
content: "hello world"
"#,
                ),
            )
            .unwrap();
        let ctx = PluginContext {
            bus: Arc::clone(&bus),
            project_root: dir.path().to_path_buf(),
            cursor_store: std::sync::Arc::new(
                crate::connectors::cursor_store::SqliteCursorStore::in_memory(),
            ),
        };
        let handle = connector.start("tick".into(), ctx).await.unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("event")
            .expect("some");

        match &ev.kind {
            EventKind::UserMessageReceived { content, connector } => {
                assert_eq!(connector, "tick");
                if let Message::User(t) = content {
                    assert_eq!(t, "hello world");
                } else {
                    panic!("unexpected message variant");
                }
            }
            other => panic!("unexpected kind: {:?}", other.tag()),
        }
        assert_eq!(ev.stream_id, "tick");

        handle.shutdown.cancel();
        let _ = handle.join.await;
    }

    #[tokio::test]
    async fn inputs_map_drives_pipeline_requested() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteEventStore::new(&dir.path().join("events.db")).unwrap());
        let bus = Arc::new(EventBus::new(store.clone()));
        let mut rx = bus.subscribe().await;

        let connector = CronBuilder
            .build(
                "tick".into(),
                yaml(
                    r#"
type: cron
cron: "* * * * * *"
pipeline: sync
inputs:
  repo_url: "git@github.com:foo/bar"
  branch: "main"
"#,
                ),
            )
            .unwrap();
        let ctx = PluginContext {
            bus: Arc::clone(&bus),
            project_root: dir.path().to_path_buf(),
            cursor_store: std::sync::Arc::new(
                crate::connectors::cursor_store::SqliteCursorStore::in_memory(),
            ),
        };
        let handle = connector.start("tick".into(), ctx).await.unwrap();

        let mut saw_pipeline = false;
        for _ in 0..2 {
            let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("event")
                .expect("some");
            if let EventKind::PipelineRequested { pipeline, inputs } = &ev.kind {
                assert_eq!(pipeline, "sync");
                assert_eq!(inputs.get("repo_url").map(String::as_str), Some("git@github.com:foo/bar"));
                assert_eq!(inputs.get("branch").map(String::as_str), Some("main"));
                saw_pipeline = true;
                break;
            }
        }
        assert!(saw_pipeline);

        handle.shutdown.cancel();
        let _ = handle.join.await;
    }

    #[tokio::test]
    async fn legacy_content_drives_pipeline_requested() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteEventStore::new(&dir.path().join("events.db")).unwrap());
        let bus = Arc::new(EventBus::new(store.clone()));
        let mut rx = bus.subscribe().await;

        let connector = CronBuilder
            .build(
                "tick".into(),
                yaml(
                    r#"
type: cron
cron: "* * * * * *"
content: "do the thing"
pipeline: digest
"#,
                ),
            )
            .unwrap();
        let ctx = PluginContext {
            bus: Arc::clone(&bus),
            project_root: dir.path().to_path_buf(),
            cursor_store: std::sync::Arc::new(
                crate::connectors::cursor_store::SqliteCursorStore::in_memory(),
            ),
        };
        let handle = connector.start("tick".into(), ctx).await.unwrap();

        let mut saw_pipeline = false;
        for _ in 0..2 {
            let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("event")
                .expect("some");
            if let EventKind::PipelineRequested { pipeline, inputs } = &ev.kind {
                assert_eq!(pipeline, "digest");
                assert_eq!(inputs.get("message").map(String::as_str), Some("do the thing"));
                saw_pipeline = true;
                break;
            }
        }
        assert!(saw_pipeline);

        handle.shutdown.cancel();
        let _ = handle.join.await;
    }
}
