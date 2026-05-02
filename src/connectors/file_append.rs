//! `type: file_append` — local-file outbound sink (ADR-0021 slice 5).
//!
//! Subscribes to the bus, filters on `on: [EventKind]`, renders a
//! MiniJinja template over the flat event context, and appends the
//! result (plus a newline if `newline: true`, the default) to `path:`.
//!
//! Path is resolved relative to the project root. Parent directories
//! must already exist — we do not auto-create, so a typo'd path fails
//! visibly on the first write rather than silently dumping into a
//! surprise location.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use minijinja::Environment;
use serde::Deserialize;
use serde_yml::Value as YamlValue;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::connectors::context::EventContext;
use crate::connectors::{ConnectorError, OutboundSink, PluginContext, PluginHandle};
use crate::events::EventKind;
use crate::plugin::PluginBuilder;

#[derive(Debug, Clone, Deserialize)]
struct FileAppendConfig {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    path: PathBuf,
    /// EventKind tags to react to. Empty = every event.
    #[serde(default)]
    on: Vec<String>,
    /// MiniJinja template over the flat event context. Required —
    /// without this, what would we write?
    template: String,
    #[serde(default = "default_newline")]
    newline: bool,
}

fn default_newline() -> bool {
    true
}

fn compile_on(list: &[String]) -> std::collections::HashSet<String> {
    list.iter().map(|s| normalise_kind(s)).collect()
}

fn normalise_kind(s: &str) -> String {
    if s.chars().any(|c| c == '_') {
        s.to_string()
    } else {
        let mut out = String::with_capacity(s.len() + 4);
        for (i, c) in s.chars().enumerate() {
            if c.is_uppercase() && i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        }
        out
    }
}

pub struct FileAppendBuilder;

impl PluginBuilder<dyn OutboundSink> for FileAppendBuilder {
    fn type_name(&self) -> &'static str {
        "file_append"
    }

    fn build(
        &self,
        name: String,
        entry: YamlValue,
    ) -> Result<Box<dyn OutboundSink>, Box<dyn std::error::Error + Send + Sync>> {
        let cfg: FileAppendConfig = serde_yml::from_value(entry)?;
        // Compile the template eagerly — bad syntax should fail at load,
        // not on the first matching event.
        let mut env = Environment::new();
        env.add_template_owned("body", cfg.template.clone())
            .map_err(|e| format!("template: {e}"))?;
        let filter = compile_on(&cfg.on);
        let _ = name;
        Ok(Box::new(FileAppendSink { cfg, filter }))
    }
}

struct FileAppendSink {
    cfg: FileAppendConfig,
    filter: std::collections::HashSet<String>,
}

#[async_trait]
impl OutboundSink for FileAppendSink {
    fn type_name(&self) -> &'static str {
        "file_append"
    }

    async fn start(
        &self,
        resolved_name: String,
        ctx: PluginContext,
    ) -> Result<PluginHandle, ConnectorError> {
        let path = resolve_path(&ctx.project_root, &self.cfg.path);
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task_name = resolved_name.clone();
        let cfg = self.cfg.clone();
        let filter = self.filter.clone();

        let mut rx = ctx.bus.subscribe().await;
        let task_bus = Arc::clone(&ctx.bus);

        let join = tokio::spawn(async move {
            log::info!(
                "file_append '{task_name}' started (path={}, on={:?})",
                path.display(),
                cfg.on
            );
            loop {
                tokio::select! {
                    _ = task_shutdown.cancelled() => break,
                    maybe_event = rx.recv() => {
                        let Some(event) = maybe_event else { break };
                        if !filter.is_empty() && !filter.contains(event.kind_tag()) {
                            continue;
                        }
                        let triggered_by = event.kind_tag().to_string();
                        let stream_id = event.stream_id.clone();
                        let correlation = event.correlation_id;
                        match write_one(&path, &cfg, &event).await {
                            Ok(()) => {
                                publish_outcome(
                                    &task_bus,
                                    &task_name,
                                    &stream_id,
                                    correlation,
                                    EventKind::OutboundDispatched {
                                        sink: task_name.clone(),
                                        sink_type: "file_append".into(),
                                        status: 200,
                                        triggered_by,
                                    },
                                )
                                .await;
                            }
                            Err(e) => {
                                log::warn!("file_append '{task_name}' write failed: {e}");
                                publish_outcome(
                                    &task_bus,
                                    &task_name,
                                    &stream_id,
                                    correlation,
                                    EventKind::OutboundFailed {
                                        sink: task_name.clone(),
                                        sink_type: "file_append".into(),
                                        reason: e,
                                        triggered_by,
                                    },
                                )
                                .await;
                            }
                        }
                    }
                }
            }
            log::info!("file_append '{task_name}' stopped");
        });

        Ok(PluginHandle {
            name: resolved_name,
            type_name: "file_append",
            shutdown,
            join,
        })
    }
}

async fn write_one(
    path: &std::path::Path,
    cfg: &FileAppendConfig,
    event: &Arc<crate::events::Event>,
) -> Result<(), String> {
    let ctx = EventContext::from_event(event);
    let mut env = Environment::new();
    env.add_template_owned("body", cfg.template.clone())
        .map_err(|e| format!("template compile: {e}"))?;
    let tmpl = env.get_template("body").map_err(|e| e.to_string())?;
    let mut rendered = tmpl
        .render(minijinja::context! { event => ctx })
        .map_err(|e| e.to_string())?;
    if cfg.newline && !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    f.write_all(rendered.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    f.flush().await.map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

async fn publish_outcome(
    bus: &Arc<crate::events::bus::EventBus>,
    sink_name: &str,
    stream_id: &str,
    correlation: Option<uuid::Uuid>,
    kind: EventKind,
) {
    let mut event = crate::events::Event::new(stream_id.to_string(), kind, sink_name.to_string());
    if let Some(c) = correlation {
        event = event.with_correlation(c);
    }
    if let Err(e) = bus.publish(event).await {
        log::warn!("file_append '{sink_name}' could not record outcome event: {e}");
    }
}

fn resolve_path(root: &std::path::Path, p: &std::path::Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::bus::EventBus;
    use crate::events::store::SqliteEventStore;
    use crate::events::Event;

    fn yaml(src: &str) -> YamlValue {
        serde_yml::from_str(src).unwrap()
    }

    #[test]
    fn build_rejects_bad_template() {
        match FileAppendBuilder.build(
            "log".into(),
            yaml("type: file_append\npath: out.log\ntemplate: \"{{ unclosed\"\n"),
        ) {
            Ok(_) => panic!("expected build failure"),
            Err(e) => assert!(e.to_string().contains("template"), "got {e}"),
        }
    }

    #[tokio::test]
    async fn appends_rendered_template_for_matching_events() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.log");
        let store = Arc::new(SqliteEventStore::new(&dir.path().join("events.db")).unwrap());
        let bus = Arc::new(EventBus::new(store.clone()));

        let cfg = format!(
            r#"
type: file_append
path: {}
on: [response_ready]
template: "[{{{{ event.stream_id }}}}] {{{{ event.content }}}}"
"#,
            out.display()
        );
        let sink = FileAppendBuilder.build("log".into(), yaml(&cfg)).unwrap();
        let ctx = PluginContext {
            bus: Arc::clone(&bus),
            project_root: dir.path().to_path_buf(),
            cursor_store: std::sync::Arc::new(
                crate::connectors::cursor_store::SqliteCursorStore::in_memory(),
            ),
        };
        let handle = sink.start("log".into(), ctx).await.unwrap();

        // Give the subscriber a tick to register before publishing.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let ev = Event::new(
            "s1".into(),
            EventKind::ResponseReady {
                conversation_id: "s1".into(),
                content: "hello".into(),
            },
            "agent".into(),
        );
        bus.publish(ev).await.unwrap();

        // Wait for the dispatch to land.
        for _ in 0..20 {
            if out.exists() && std::fs::metadata(&out).map(|m| m.len() > 0).unwrap_or(false) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let body = std::fs::read_to_string(&out).unwrap();
        assert_eq!(body, "[s1] hello\n");

        handle.shutdown.cancel();
        let _ = handle.join.await;
    }

    #[tokio::test]
    async fn skips_events_outside_filter() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.log");
        let store = Arc::new(SqliteEventStore::new(&dir.path().join("events.db")).unwrap());
        let bus = Arc::new(EventBus::new(store.clone()));

        let cfg = format!(
            "type: file_append\npath: {}\non: [response_ready]\ntemplate: \"X\"\n",
            out.display()
        );
        let sink = FileAppendBuilder.build("log".into(), yaml(&cfg)).unwrap();
        let ctx = PluginContext {
            bus: Arc::clone(&bus),
            project_root: dir.path().to_path_buf(),
            cursor_store: std::sync::Arc::new(
                crate::connectors::cursor_store::SqliteCursorStore::in_memory(),
            ),
        };
        let handle = sink.start("log".into(), ctx).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let ev = Event::new(
            "s2".into(),
            EventKind::UserMessageReceived {
                content: crate::types::Message::User("hi".into()),
                connector: "test".into(),
            },
            "test".into(),
        );
        bus.publish(ev).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(!out.exists(), "filtered events should not write");

        handle.shutdown.cancel();
        let _ = handle.join.await;
    }
}
