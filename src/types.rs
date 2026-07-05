use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentPart {
    Text(String),
    ImageBase64 { media_type: String, data: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    System(String),
    User(String),
    UserMultimodal {
        parts: Vec<ContentPart>,
    },
    Assistant {
        content: Option<String>,
        tool_calls: Vec<ToolCallInfo>,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
    },
}

impl Message {
    /// Extract the plain text from a user message.
    pub fn user_text(&self) -> Option<&str> {
        match self {
            Message::User(t) => Some(t),
            Message::UserMultimodal { parts } => parts.iter().find_map(|p| match p {
                ContentPart::Text(t) => Some(t.as_str()),
                _ => None,
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Total input tokens for the call, cache hits included.
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Portion of `input_tokens` served from the provider's prompt cache
    /// (Anthropic `cache_read_input_tokens`, OpenAI `cached_tokens`).
    #[serde(default)]
    pub cached_input_tokens: u32,
    /// Anthropic only: tokens written to the prompt cache on this call.
    /// OpenAI caches automatically and reports no write-side counter.
    #[serde(default)]
    pub cache_creation_tokens: u32,
}

impl TokenUsage {
    /// Fraction of input tokens served from the provider's prompt cache,
    /// in `0.0..=1.0`. Returns 0.0 when no input tokens were reported.
    pub fn cache_hit_rate(&self) -> f64 {
        if self.input_tokens == 0 {
            0.0
        } else {
            f64::from(self.cached_input_tokens) / f64::from(self.input_tokens)
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum StreamEvent {
    Token(String),
    ContentDone(String),
    ToolCallStart {
        id: String,
        name: String,
        arguments: String,
    },
    ToolCallResult {
        id: String,
        name: String,
        result: String,
        is_error: bool,
    },
    IterationStart(usize),
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        message_count: usize,
        summary_threshold: usize,
    },
    TaskSpawned {
        id: String,
        description: String,
    },
    TaskUpdate {
        id: String,
        status: String,
    },
    // -- Workflow engine events --
    WorkflowAssessment {
        score: u8,
        reasoning: String,
    },
    WorkflowPlanReady {
        node_count: usize,
        edge_count: usize,
    },
    WorkflowNodeStart {
        node_id: String,
        description: String,
    },
    WorkflowNodeComplete {
        node_id: String,
        success: bool,
    },
    WorkflowProgress {
        completed: usize,
        total: usize,
    },
    WorkflowTraceReady {
        summary: String,
        trace_path: Option<String>,
    },
    Done(String),
    Error(String),
}
