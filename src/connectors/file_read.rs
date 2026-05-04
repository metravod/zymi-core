//! `type: file_read` — local-file event source (ADR-0021 slice 5).
//!
//! Reads `path:` once at startup and synthesises events. Two modes:
//!
//! * `mode: lines` (default) — one [`EventKind::UserMessageReceived`] per
//!   non-empty line. Useful as a batch driver: 100 prompts in a `.txt`,
//!   each runs through the pipeline.
//! * `mode: whole` — single event whose content is the entire file body.
//!
//! No tailing, no cursor, no inotify. Tail-style use cases compose `cron`
//! + `file_read` if needed; the primitive stays narrow.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_yml::Value as YamlValue;
use tokio_util::sync::CancellationToken;

use crate::connectors::{ConnectorError, InboundConnector, PluginContext, PluginHandle};
use crate::events::{Event, EventKind};
use crate::plugin::PluginBuilder;
use crate::types::Message;

#[derive(Debug, Clone, Deserialize)]
struct FileReadConfig {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    path: PathBuf,
    #[serde(default)]
    mode: ReadMode,
    /// Synthetic stream id. Defaults to connector name.
    #[serde(default)]
    stream_id: Option<String>,
    #[serde(default)]
    pipeline: Option<String>,
    #[serde(default = "default_pipeline_input")]
    pipeline_input: String,
}

fn default_pipeline_input() -> String {
    "message".into()
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum ReadMode {
    #[default]
    Lines,
    Whole,
}

pub struct FileReadBuilder;

impl PluginBuilder<dyn InboundConnector> for FileReadBuilder {
    fn type_name(&self) -> &'static str {
        "file_read"
    }

    fn build(
        &self,
        name: String,
        entry: YamlValue,
    ) -> Result<Box<dyn InboundConnector>, Box<dyn std::error::Error + Send + Sync>> {
        let cfg: FileReadConfig = serde_yml::from_value(entry)?;
        let _ = name;
        Ok(Box::new(FileReadConnector { cfg }))
    }
}

struct FileReadConnector {
    cfg: FileReadConfig,
}

#[async_trait]
impl InboundConnector for FileReadConnector {
    fn type_name(&self) -> &'static str {
        "file_read"
    }

    async fn start(
        &self,
        resolved_name: String,
        ctx: PluginContext,
    ) -> Result<PluginHandle, ConnectorError> {
        let path = resolve_path(&ctx.project_root, &self.cfg.path);
        let body = tokio::fs::read_to_string(&path).await.map_err(|e| {
            ConnectorError::InvalidConfig(format!(
                "file_read '{resolved_name}': cannot read {}: {e}",
                path.display()
            ))
        })?;

        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task_name = resolved_name.clone();
        let task_ctx = ctx.clone();
        let cfg = self.cfg.clone();

        let join = tokio::spawn(async move {
            log::info!(
                "file_read '{task_name}' started ({} bytes from {})",
                body.len(),
                path.display()
            );
            // We publish synchronously (no schedule) so abandoning early
            // is fine — but check the shutdown token between batches in
            // case the runtime tears down before we finish.
            match cfg.mode {
                ReadMode::Lines => {
                    for line in body.lines() {
                        if task_shutdown.is_cancelled() {
                            break;
                        }
                        let trimmed = line.trim_end_matches('\r');
                        if trimmed.is_empty() {
                            continue;
                        }
                        emit(&task_name, &cfg, &task_ctx, trimmed).await;
                    }
                }
                ReadMode::Whole => {
                    if !task_shutdown.is_cancelled() {
                        emit(&task_name, &cfg, &task_ctx, &body).await;
                    }
                }
            }
            log::info!("file_read '{task_name}' finished");
        });

        Ok(PluginHandle {
            name: resolved_name,
            type_name: "file_read",
            shutdown,
            join,
        })
    }
}

fn resolve_path(root: &std::path::Path, p: &std::path::Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

async fn emit(name: &str, cfg: &FileReadConfig, ctx: &PluginContext, content: &str) {
    let stream_id = cfg.stream_id.clone().unwrap_or_else(|| name.to_string());
    let correlation = uuid::Uuid::new_v4();
    let event = Event::new(
        stream_id.clone(),
        EventKind::UserMessageReceived {
            content: Message::User(content.to_string()),
            connector: name.to_string(),
        },
        name.to_string(),
    )
    .with_correlation(correlation);

    if let Err(e) = ctx.bus.publish(event).await {
        log::warn!("file_read '{name}' publish failed: {e}");
        return;
    }

    if let Some(pipeline) = cfg.pipeline.as_deref() {
        let mut inputs = HashMap::new();
        inputs.insert(cfg.pipeline_input.clone(), content.to_string());
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
            log::warn!("file_read '{name}' pipeline publish failed: {e}");
        }
    }
}

// Suppress unused-import lints when the test cfg drops Arc references.
#[allow(dead_code)]
fn _arc_check(_b: Arc<crate::events::bus::EventBus>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::bus::EventBus;
    use crate::events::store::SqliteEventStore;
    use std::io::Write;

    fn yaml(src: &str) -> YamlValue {
        serde_yml::from_str(src).unwrap()
    }

    #[tokio::test]
    async fn lines_mode_emits_one_event_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("inputs.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        writeln!(f, "first").unwrap();
        writeln!(f, "second").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "third").unwrap();
        drop(f);

        let store = Arc::new(SqliteEventStore::new(&dir.path().join("events.db")).unwrap());
        let bus = Arc::new(EventBus::new(store.clone()));
        let mut rx = bus.subscribe().await;

        let yaml_cfg = format!(
            "type: file_read\npath: {}\nmode: lines\n",
            file_path.display()
        );
        let connector = FileReadBuilder
            .build("inputs".into(), yaml(&yaml_cfg))
            .unwrap();
        let ctx = PluginContext {
            bus: Arc::clone(&bus),
            project_root: dir.path().to_path_buf(),
            cursor_store: std::sync::Arc::new(
                crate::connectors::cursor_store::SqliteCursorStore::in_memory(),
            ),
        };
        let handle = connector.start("inputs".into(), ctx).await.unwrap();

        let mut got = Vec::new();
        for _ in 0..3 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("event")
                .expect("some");
            if let EventKind::UserMessageReceived {
                content: Message::User(t),
                ..
            } = &ev.kind
            {
                got.push(t.clone());
            }
        }
        assert_eq!(got, vec!["first", "second", "third"]);

        handle.shutdown.cancel();
        let _ = handle.join.await;
    }

    #[tokio::test]
    async fn whole_mode_emits_single_event_with_full_body() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("script.txt");
        std::fs::write(&file_path, "line one\nline two\n").unwrap();

        let store = Arc::new(SqliteEventStore::new(&dir.path().join("events.db")).unwrap());
        let bus = Arc::new(EventBus::new(store.clone()));
        let mut rx = bus.subscribe().await;

        let yaml_cfg = format!(
            "type: file_read\npath: {}\nmode: whole\n",
            file_path.display()
        );
        let connector = FileReadBuilder
            .build("doc".into(), yaml(&yaml_cfg))
            .unwrap();
        let ctx = PluginContext {
            bus: Arc::clone(&bus),
            project_root: dir.path().to_path_buf(),
            cursor_store: std::sync::Arc::new(
                crate::connectors::cursor_store::SqliteCursorStore::in_memory(),
            ),
        };
        let handle = connector.start("doc".into(), ctx).await.unwrap();

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("event")
            .expect("some");
        if let EventKind::UserMessageReceived {
            content: Message::User(t),
            ..
        } = &ev.kind
        {
            assert_eq!(t, "line one\nline two\n");
        } else {
            panic!("unexpected event");
        }

        handle.shutdown.cancel();
        let _ = handle.join.await;
    }

    #[tokio::test]
    async fn missing_file_fails_at_start() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteEventStore::new(&dir.path().join("events.db")).unwrap());
        let bus = Arc::new(EventBus::new(store.clone()));

        let connector = FileReadBuilder
            .build(
                "missing".into(),
                yaml("type: file_read\npath: nope.txt\n"),
            )
            .unwrap();
        let ctx = PluginContext {
            bus,
            project_root: dir.path().to_path_buf(),
            cursor_store: std::sync::Arc::new(
                crate::connectors::cursor_store::SqliteCursorStore::in_memory(),
            ),
        };
        match connector.start("missing".into(), ctx).await {
            Ok(_) => panic!("expected start failure"),
            Err(ConnectorError::InvalidConfig(msg)) => assert!(msg.contains("cannot read"), "got {msg}"),
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
}
