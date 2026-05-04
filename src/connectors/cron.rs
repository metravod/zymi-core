//! `type: cron` — timer-driven synthetic event source (ADR-0021 slice 4).
//!
//! Publishes a fixed `content` as [`EventKind::UserMessageReceived`] on a
//! schedule. Two scheduling forms are supported:
//!
//! * `every_secs: N` — fires every N seconds (first tick after one interval).
//! * `at: "HH:MM"` — fires once per day at the given local time.
//!
//! When `pipeline:` is set, every tick additionally publishes a
//! [`EventKind::PipelineRequested`] so `zymi serve <pipeline>` picks it up
//! without extra glue, mirroring `http_inbound` / `http_poll`.
//!
//! No network involved — this is a clock, not an integration.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{Local, NaiveTime};
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
    /// Fixed-interval schedule. Mutually exclusive with `at:`.
    #[serde(default)]
    every_secs: Option<u64>,
    /// Daily-at-time schedule, `HH:MM` in local time. Mutually exclusive
    /// with `every_secs:`.
    #[serde(default)]
    at: Option<String>,
    /// Synthetic stream id. Defaults to the connector name.
    #[serde(default)]
    stream_id: Option<String>,
    /// Synthetic message content. Required — this is the whole point.
    content: String,
    /// Optional: also publish `PipelineRequested` per tick.
    #[serde(default)]
    pipeline: Option<String>,
    #[serde(default = "default_pipeline_input")]
    pipeline_input: String,
}

fn default_pipeline_input() -> String {
    "message".into()
}

#[derive(Debug, Clone)]
enum Schedule {
    Every(Duration),
    DailyAt(NaiveTime),
}

impl Schedule {
    fn from_config(cfg: &CronConfig) -> Result<Self, ConnectorError> {
        match (cfg.every_secs, cfg.at.as_deref()) {
            (Some(_), Some(_)) => Err(ConnectorError::InvalidConfig(
                "set either 'every_secs' or 'at', not both".into(),
            )),
            (None, None) => Err(ConnectorError::InvalidConfig(
                "cron requires 'every_secs' or 'at'".into(),
            )),
            (Some(0), _) => Err(ConnectorError::InvalidConfig(
                "every_secs must be > 0".into(),
            )),
            (Some(n), None) => Ok(Schedule::Every(Duration::from_secs(n))),
            (None, Some(s)) => {
                let t = NaiveTime::parse_from_str(s, "%H:%M").map_err(|e| {
                    ConnectorError::InvalidConfig(format!(
                        "at: invalid HH:MM '{s}': {e}"
                    ))
                })?;
                Ok(Schedule::DailyAt(t))
            }
        }
    }
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
        let schedule = Schedule::from_config(&cfg)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        let _ = name;
        Ok(Box::new(CronConnector { cfg, schedule }))
    }
}

struct CronConnector {
    cfg: CronConfig,
    schedule: Schedule,
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
    schedule: Schedule,
    ctx: PluginContext,
    shutdown: CancellationToken,
) {
    log::info!("cron '{name}' started ({})", describe_schedule(&schedule));
    match schedule {
        Schedule::Every(d) => {
            let mut interval = tokio::time::interval(d);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // First tick fires immediately — skip it so the first real
            // emit waits one interval. Matches `http_poll`.
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = interval.tick() => {
                        emit(&name, &cfg, &ctx).await;
                    }
                }
            }
        }
        Schedule::DailyAt(t) => loop {
            let wait = duration_until_next(t);
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(wait) => {
                    emit(&name, &cfg, &ctx).await;
                }
            }
        },
    }
    log::info!("cron '{name}' stopped");
}

fn describe_schedule(s: &Schedule) -> String {
    match s {
        Schedule::Every(d) => format!("every {}s", d.as_secs()),
        Schedule::DailyAt(t) => format!("daily at {}", t.format("%H:%M")),
    }
}

fn duration_until_next(target: NaiveTime) -> Duration {
    let now = Local::now();
    let today = now.date_naive().and_time(target);
    let next = if today > now.naive_local() {
        today
    } else {
        today + chrono::Duration::days(1)
    };
    let delta = next.and_local_timezone(Local).single().map(|dt| dt - now);
    delta
        .and_then(|d| d.to_std().ok())
        .unwrap_or(Duration::from_secs(60))
}

async fn emit(name: &str, cfg: &CronConfig, ctx: &PluginContext) {
    let stream_id = cfg.stream_id.clone().unwrap_or_else(|| name.to_string());
    let correlation = uuid::Uuid::new_v4();
    let event = Event::new(
        stream_id.clone(),
        EventKind::UserMessageReceived {
            content: Message::User(cfg.content.clone()),
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
        let mut inputs = HashMap::new();
        inputs.insert(cfg.pipeline_input.clone(), cfg.content.clone());
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
    use chrono::Timelike;

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
    fn build_requires_schedule_field() {
        let err = expect_build_err(yaml("type: cron\ncontent: hi"));
        assert!(err.contains("every_secs"), "got {err}");
    }

    #[test]
    fn build_rejects_both_schedule_fields() {
        let err = expect_build_err(yaml(
            "type: cron\nevery_secs: 60\nat: \"09:00\"\ncontent: hi",
        ));
        assert!(err.contains("either"), "got {err}");
    }

    #[test]
    fn build_rejects_zero_every_secs() {
        let err = expect_build_err(yaml("type: cron\nevery_secs: 0\ncontent: hi"));
        assert!(err.contains("> 0"), "got {err}");
    }

    #[test]
    fn build_rejects_invalid_at() {
        let err = expect_build_err(yaml("type: cron\nat: \"not a time\"\ncontent: hi"));
        assert!(err.contains("HH:MM"), "got {err}");
    }

    #[test]
    fn duration_until_next_is_in_future() {
        let now = Local::now();
        let later = NaiveTime::from_hms_opt(
            (now.hour() + 1) % 24,
            now.minute(),
            0,
        )
        .unwrap();
        let d = duration_until_next(later);
        assert!(d.as_secs() > 0);
        assert!(d.as_secs() <= 24 * 3600);
    }

    #[tokio::test]
    async fn every_secs_emits_user_message_received() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteEventStore::new(&dir.path().join("events.db")).unwrap());
        let bus = Arc::new(EventBus::new(store.clone()));
        let mut rx = bus.subscribe().await;

        // Use a sub-second interval — the test budget is real time. The
        // connector's "skip first immediate tick" rule means the first
        // emit lands one interval after start.
        let connector = CronBuilder
            .build(
                "tick".into(),
                yaml(
                    r#"
type: cron
every_secs: 1
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
    async fn pipeline_field_publishes_pipeline_requested() {
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
every_secs: 1
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

        let mut saw_user = false;
        let mut saw_pipeline = false;
        for _ in 0..2 {
            let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("event")
                .expect("some");
            match &ev.kind {
                EventKind::UserMessageReceived { .. } => saw_user = true,
                EventKind::PipelineRequested { pipeline, inputs } => {
                    assert_eq!(pipeline, "digest");
                    assert_eq!(inputs.get("message").map(String::as_str), Some("do the thing"));
                    saw_pipeline = true;
                }
                other => panic!("unexpected kind: {:?}", other.tag()),
            }
        }
        assert!(saw_user && saw_pipeline);

        handle.shutdown.cancel();
        let _ = handle.join.await;
    }
}
