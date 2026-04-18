//! Seed the event store with two synthetic pipeline runs for demoing
//! `zymi runs` / `zymi observe` without running real LLM calls.
//!
//! Usage:
//!   cargo run --features cli --example seed_observe -- <project-dir>

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use uuid::Uuid;
use zymi_core::events::store::{open_store, StoreBackend};
use zymi_core::events::{Event, EventKind};
use zymi_core::types::{Message, TokenUsage, ToolCallInfo};

#[tokio::main]
async fn main() -> Result<(), String> {
    let project_dir: PathBuf = std::env::args()
        .nth(1)
        .ok_or_else(|| "usage: seed_observe <project-dir>".to_string())?
        .into();

    let db_path = project_dir.join(".zymi").join("events.db");
    std::fs::create_dir_all(db_path.parent().unwrap())
        .map_err(|e| format!("mkdir {}: {e}", db_path.display()))?;

    let store = open_store(StoreBackend::Sqlite { path: db_path.clone() })
        .map_err(|e| format!("open store: {e}"))?;

    // Run 1: research pipeline — succeeded, full diamond.
    seed_run(
        &*store,
        "research",
        &["search", "analyze", "summarize", "review"],
        true,
    )
    .await?;

    tokio::time::sleep(Duration::from_millis(10)).await;

    // Run 2: research pipeline — failed at `analyze`, downstream never ran.
    seed_failed_run(&*store, "research", "analyze").await?;

    tokio::time::sleep(Duration::from_millis(10)).await;

    // Run 3: main pipeline — still running (no PipelineCompleted).
    seed_running_run(&*store, "main", &["process"]).await?;

    println!("seeded 3 demo runs into {}", db_path.display());
    Ok(())
}

async fn seed_run(
    store: &dyn zymi_core::events::store::EventStore,
    pipeline: &str,
    node_ids: &[&str],
    success: bool,
) -> Result<(), String> {
    let stream_id = format!("pipeline-{pipeline}-{}", Uuid::new_v4());
    let corr = Uuid::new_v4();

    let mut inputs = HashMap::new();
    inputs.insert("topic".into(), "event sourcing in Rust".into());
    append(
        store,
        &stream_id,
        EventKind::PipelineRequested {
            pipeline: pipeline.into(),
            inputs,
        },
        Some(corr),
    )
    .await?;

    append(
        store,
        &stream_id,
        EventKind::WorkflowStarted {
            user_message: format!("pipeline: {pipeline}"),
            node_count: node_ids.len(),
        },
        Some(corr),
    )
    .await?;

    for node_id in node_ids {
        append(
            store,
            &stream_id,
            EventKind::WorkflowNodeStarted {
                node_id: (*node_id).into(),
                description: format!("run step {node_id}"),
            },
            Some(corr),
        )
        .await?;

        append(
            store,
            &stream_id,
            EventKind::LlmCallStarted {
                iteration: 0,
                message_count: 3,
                approx_context_chars: 2048,
            },
            Some(corr),
        )
        .await?;

        append(
            store,
            &stream_id,
            EventKind::LlmCallCompleted {
                has_tool_calls: false,
                usage: Some(TokenUsage {
                    input_tokens: 120,
                    output_tokens: 48,
                }),
                response_message: Some(Message::Assistant {
                    content: Some(format!("result of {node_id}")),
                    tool_calls: Vec::<ToolCallInfo>::new(),
                }),
                content_preview: Some(format!("result of {node_id}")),
            },
            Some(corr),
        )
        .await?;

        append(
            store,
            &stream_id,
            EventKind::WorkflowNodeCompleted {
                node_id: (*node_id).into(),
                success: true,
            },
            Some(corr),
        )
        .await?;
    }

    append(
        store,
        &stream_id,
        EventKind::WorkflowCompleted { success },
        Some(corr),
    )
    .await?;

    append(
        store,
        &stream_id,
        EventKind::PipelineCompleted {
            pipeline: pipeline.into(),
            success,
            final_output: Some("summary: event sourcing rocks".into()),
            error: None,
        },
        Some(corr),
    )
    .await?;

    Ok(())
}

async fn seed_failed_run(
    store: &dyn zymi_core::events::store::EventStore,
    pipeline: &str,
    fail_at: &str,
) -> Result<(), String> {
    let stream_id = format!("pipeline-{pipeline}-{}", Uuid::new_v4());
    let corr = Uuid::new_v4();

    let mut inputs = HashMap::new();
    inputs.insert("topic".into(), "provider rate limit demo".into());
    append(
        store,
        &stream_id,
        EventKind::PipelineRequested {
            pipeline: pipeline.into(),
            inputs,
        },
        Some(corr),
    )
    .await?;

    append(
        store,
        &stream_id,
        EventKind::WorkflowStarted {
            user_message: format!("pipeline: {pipeline}"),
            node_count: 4,
        },
        Some(corr),
    )
    .await?;

    // search succeeds
    append(
        store,
        &stream_id,
        EventKind::WorkflowNodeStarted {
            node_id: "search".into(),
            description: "run step search".into(),
        },
        Some(corr),
    )
    .await?;
    append(
        store,
        &stream_id,
        EventKind::WorkflowNodeCompleted {
            node_id: "search".into(),
            success: true,
        },
        Some(corr),
    )
    .await?;

    // analyze fails
    append(
        store,
        &stream_id,
        EventKind::WorkflowNodeStarted {
            node_id: fail_at.into(),
            description: "run step analyze".into(),
        },
        Some(corr),
    )
    .await?;
    append(
        store,
        &stream_id,
        EventKind::ToolCallRequested {
            tool_name: "web_search".into(),
            arguments: "{\"q\":\"rust event sourcing\"}".into(),
            call_id: "tc-1".into(),
        },
        Some(corr),
    )
    .await?;
    append(
        store,
        &stream_id,
        EventKind::ToolCallCompleted {
            call_id: "tc-1".into(),
            result: "rate limited".into(),
            result_preview: "rate limited".into(),
            is_error: true,
            duration_ms: 4321,
        },
        Some(corr),
    )
    .await?;
    append(
        store,
        &stream_id,
        EventKind::WorkflowNodeCompleted {
            node_id: fail_at.into(),
            success: false,
        },
        Some(corr),
    )
    .await?;

    append(
        store,
        &stream_id,
        EventKind::WorkflowCompleted { success: false },
        Some(corr),
    )
    .await?;
    append(
        store,
        &stream_id,
        EventKind::PipelineCompleted {
            pipeline: pipeline.into(),
            success: false,
            final_output: None,
            error: Some(format!("step {fail_at} failed: rate limited")),
        },
        Some(corr),
    )
    .await?;
    Ok(())
}

async fn seed_running_run(
    store: &dyn zymi_core::events::store::EventStore,
    pipeline: &str,
    node_ids: &[&str],
) -> Result<(), String> {
    let stream_id = format!("pipeline-{pipeline}-{}", Uuid::new_v4());
    let corr = Uuid::new_v4();

    let mut inputs = HashMap::new();
    inputs.insert("topic".into(), "long-running demo".into());
    append(
        store,
        &stream_id,
        EventKind::PipelineRequested {
            pipeline: pipeline.into(),
            inputs,
        },
        Some(corr),
    )
    .await?;

    append(
        store,
        &stream_id,
        EventKind::WorkflowStarted {
            user_message: format!("pipeline: {pipeline}"),
            node_count: node_ids.len(),
        },
        Some(corr),
    )
    .await?;

    for node_id in node_ids {
        append(
            store,
            &stream_id,
            EventKind::WorkflowNodeStarted {
                node_id: (*node_id).into(),
                description: format!("run step {node_id}"),
            },
            Some(corr),
        )
        .await?;
    }

    Ok(())
}

async fn append(
    store: &dyn zymi_core::events::store::EventStore,
    stream_id: &str,
    kind: EventKind,
    corr: Option<Uuid>,
) -> Result<(), String> {
    let mut event = Event::new(stream_id.to_string(), kind, "demo".into());
    if let Some(c) = corr {
        event.correlation_id = Some(c);
    }
    store
        .append(&mut event)
        .await
        .map_err(|e| format!("append: {e}"))?;
    Ok(())
}
