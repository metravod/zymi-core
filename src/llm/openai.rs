use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::types::{Message, TokenUsage, ToolCallInfo};

use super::error::LlmError;
use super::{ChatRequest, ChatResponse, LlmProvider};

/// OpenAI-compatible provider. Works with OpenAI, vLLM, Ollama, Together, and
/// any other service that implements the `/v1/chat/completions` endpoint.
#[derive(Debug)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    stream: bool,
}

/// A non-streamed request that fails after at least this long without a
/// response is reported as [`LlmError::Dropped`] (gateway timeout hint).
const DROPPED_AFTER: std::time::Duration = std::time::Duration::from_secs(45);

impl OpenAiProvider {
    pub fn new(base_url: String, api_key: Option<String>, model: String) -> Self {
        Self {
            client: super::http_client(),
            base_url,
            api_key,
            model,
            stream: false,
        }
    }

    /// Request the completion as an SSE stream (`LlmConfig::stream`).
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat_completion(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let mut oai_request = build_request(&self.model, request);
        if self.stream {
            oai_request.stream = Some(true);
            oai_request.stream_options = Some(serde_json::json!({ "include_usage": true }));
        }
        let url = format!("{}/chat/completions", self.base_url);

        let mut http = self.client.post(&url);
        if let Some(key) = &self.api_key {
            http = http.bearer_auth(key);
        }

        let started = std::time::Instant::now();
        let dropped = |e: reqwest::Error| -> LlmError {
            let elapsed = started.elapsed();
            if !self.stream && elapsed >= DROPPED_AFTER && !e.is_timeout() {
                LlmError::Dropped {
                    url: url.clone(),
                    elapsed_secs: elapsed.as_secs(),
                    cause: super::error::source_chain(&e),
                }
            } else {
                LlmError::Http(e)
            }
        };

        let mut resp = http.json(&oai_request).send().await.map_err(dropped)?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status,
                message: body,
            });
        }

        if !self.stream {
            let oai_resp: OaiResponse = resp
                .json()
                .await
                .map_err(|e| LlmError::Serialization(e.to_string()))?;
            return parse_response(oai_resp);
        }

        let mut acc = StreamAcc::default();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                if acc.feed_line(line.trim_end())? {
                    return parse_response(acc.finish());
                }
            }
        }
        if !buf.is_empty() {
            acc.feed_line(String::from_utf8_lossy(&buf).trim_end())?;
        }
        parse_response(acc.finish())
    }
}

// ---------------------------------------------------------------------------
// Streaming (SSE) — accumulated into the same OaiResponse the plain path
// parses, so the rest of the provider can't tell the difference.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct StreamAcc {
    model: String,
    content: String,
    saw_content: bool,
    tool_calls: Vec<OaiToolCall>,
    usage: Option<OaiUsage>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OaiChunk {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<OaiChunkChoice>,
    #[serde(default)]
    usage: Option<OaiUsage>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct OaiChunkChoice {
    #[serde(default)]
    delta: Option<OaiDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OaiDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OaiDeltaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OaiDeltaToolCall {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OaiDeltaFunction>,
}

#[derive(Debug, Deserialize)]
struct OaiDeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

impl StreamAcc {
    /// Feed one SSE line. Returns `true` on the `[DONE]` sentinel.
    fn feed_line(&mut self, line: &str) -> Result<bool, LlmError> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(false); // blank separators, `event:` / `:` comments
        };
        let data = data.trim();
        if data == "[DONE]" {
            return Ok(true);
        }
        if data.is_empty() {
            return Ok(false);
        }
        let chunk: OaiChunk = serde_json::from_str(data)
            .map_err(|e| LlmError::Serialization(format!("bad stream chunk ({e}): {data}")))?;
        if let Some(err) = chunk.error {
            return Err(LlmError::Api {
                status: 200,
                message: err.to_string(),
            });
        }
        if let Some(model) = chunk.model {
            self.model = model;
        }
        if chunk.usage.is_some() {
            self.usage = chunk.usage;
        }
        for choice in chunk.choices {
            if choice.finish_reason.is_some() {
                self.finish_reason = choice.finish_reason;
            }
            let Some(delta) = choice.delta else { continue };
            if let Some(text) = delta.content {
                self.saw_content = true;
                self.content.push_str(&text);
            }
            for tc in delta.tool_calls.unwrap_or_default() {
                let idx = tc.index.unwrap_or(self.tool_calls.len().saturating_sub(1));
                while self.tool_calls.len() <= idx {
                    self.tool_calls.push(OaiToolCall {
                        id: String::new(),
                        r#type: "function".into(),
                        function: OaiFunction {
                            name: String::new(),
                            arguments: String::new(),
                        },
                    });
                }
                let slot = &mut self.tool_calls[idx];
                if let Some(id) = tc.id {
                    slot.id = id;
                }
                if let Some(f) = tc.function {
                    if let Some(name) = f.name {
                        slot.function.name.push_str(&name);
                    }
                    if let Some(args) = f.arguments {
                        slot.function.arguments.push_str(&args);
                    }
                }
            }
        }
        Ok(false)
    }

    fn finish(self) -> OaiResponse {
        OaiResponse {
            model: self.model,
            choices: vec![OaiChoice {
                message: OaiMessage {
                    role: "assistant".into(),
                    content: self.saw_content.then_some(self.content),
                    tool_calls: (!self.tool_calls.is_empty()).then_some(self.tool_calls),
                    tool_call_id: None,
                },
                finish_reason: self.finish_reason,
            }],
            usage: self.usage,
        }
    }
}

// ---------------------------------------------------------------------------
// Wire types — OpenAI chat completions API
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct OaiRequest {
    model: String,
    messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OaiTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OaiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct OaiToolCall {
    id: String,
    r#type: String,
    function: OaiFunction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct OaiFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct OaiTool {
    r#type: String,
    function: OaiToolFunction,
}

#[derive(Debug, Serialize)]
struct OaiToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OaiResponse {
    model: String,
    choices: Vec<OaiChoice>,
    #[serde(default)]
    usage: Option<OaiUsage>,
}

#[derive(Debug, Deserialize)]
struct OaiChoice {
    message: OaiMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OaiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<OaiPromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct OaiPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

/// Newer OpenAI models (reasoning family) reject `max_tokens` (use
/// `max_completion_tokens`) and do not accept custom `temperature`.
fn is_reasoning_model(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("o1") || m.starts_with("o3") || m.starts_with("gpt-5") || m.starts_with("gpt-4.5")
}

fn build_request(model: &str, request: &ChatRequest) -> OaiRequest {
    let messages = request.messages.iter().map(message_to_oai).collect();

    let tools = request
        .tools
        .iter()
        .map(|t| OaiTool {
            r#type: "function".into(),
            function: OaiToolFunction {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            },
        })
        .collect();

    let reasoning = is_reasoning_model(model);

    let (max_tokens, max_completion_tokens) = if reasoning {
        (None, request.max_tokens)
    } else {
        (request.max_tokens, None)
    };

    // Reasoning models only accept the default temperature (1).
    let temperature = if reasoning { None } else { request.temperature };

    OaiRequest {
        model: model.into(),
        messages,
        tools,
        temperature,
        max_tokens,
        max_completion_tokens,
        stream: None,
        stream_options: None,
    }
}

fn message_to_oai(msg: &Message) -> OaiMessage {
    match msg {
        Message::System(text) => OaiMessage {
            role: "system".into(),
            content: Some(text.clone()),
            tool_calls: None,
            tool_call_id: None,
        },
        Message::User(text) => OaiMessage {
            role: "user".into(),
            content: Some(text.clone()),
            tool_calls: None,
            tool_call_id: None,
        },
        Message::UserMultimodal { parts } => {
            // Flatten to text-only for the chat completions API.
            let text = parts
                .iter()
                .filter_map(|p| match p {
                    crate::types::ContentPart::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            OaiMessage {
                role: "user".into(),
                content: Some(text),
                tool_calls: None,
                tool_call_id: None,
            }
        }
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let oai_tool_calls = if tool_calls.is_empty() {
                None
            } else {
                Some(
                    tool_calls
                        .iter()
                        .map(|tc| OaiToolCall {
                            id: tc.id.clone(),
                            r#type: "function".into(),
                            function: OaiFunction {
                                name: tc.name.clone(),
                                arguments: tc.arguments.clone(),
                            },
                        })
                        .collect(),
                )
            };
            OaiMessage {
                role: "assistant".into(),
                content: content.clone(),
                tool_calls: oai_tool_calls,
                tool_call_id: None,
            }
        }
        Message::ToolResult {
            tool_call_id,
            content,
        } => OaiMessage {
            role: "tool".into(),
            content: Some(content.clone()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.clone()),
        },
    }
}

fn parse_response(resp: OaiResponse) -> Result<ChatResponse, LlmError> {
    let choice = resp
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| LlmError::Serialization("empty choices array".into()))?;

    let finish_reason = choice.finish_reason;
    let tool_calls = choice
        .message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|tc| ToolCallInfo {
            id: tc.id,
            name: tc.function.name,
            arguments: tc.function.arguments,
        })
        .collect();

    let message = Message::Assistant {
        content: choice.message.content,
        tool_calls,
    };

    let usage = resp
        .usage
        .map(|u| TokenUsage {
            // OpenAI's `prompt_tokens` already includes cached tokens.
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cached_input_tokens: u
                .prompt_tokens_details
                .as_ref()
                .map_or(0, |d| d.cached_tokens),
            cache_creation_tokens: 0,
        })
        .unwrap_or_default();

    Ok(ChatResponse {
        message,
        usage,
        model: resp.model,
        finish_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolDefinition;

    fn feed_all(sse: &str) -> ChatResponse {
        let mut acc = StreamAcc::default();
        for line in sse.lines() {
            if acc.feed_line(line).unwrap() {
                break;
            }
        }
        parse_response(acc.finish()).unwrap()
    }

    #[test]
    fn stream_accumulates_text_and_usage() {
        // Shape captured from an OpenAI-compatible gateway (vLLM behind a
        // proxy): role chunk, content deltas, finish chunk, usage-only chunk.
        let sse = r#"data: {"id":"c1","model":"qwen3.8-27b","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}

data: {"id":"c1","model":"qwen3.8-27b","choices":[{"index":0,"delta":{"content":"Рейк"}}]}

data: {"id":"c1","model":"qwen3.8-27b","choices":[{"index":0,"delta":{"content":"ьявик"}}]}

data: {"id":"c1","model":"qwen3.8-27b","choices":[{"finish_reason":"stop","index":0,"delta":{}}]}

data: {"id":"c1","model":"qwen3.8-27b","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3}}

data: [DONE]
"#;
        let resp = feed_all(sse);
        assert_eq!(resp.model, "qwen3.8-27b");
        match resp.message {
            Message::Assistant { content, tool_calls } => {
                assert_eq!(content.as_deref(), Some("Рейкьявик"));
                assert!(tool_calls.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 3);
    }

    #[test]
    fn stream_accumulates_tool_call_fragments() {
        let sse = r#"data: {"model":"m","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":""}}]}}]}
data: {"model":"m","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}
data: {"model":"m","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.md\"}"}}]}}]}
data: {"model":"m","choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_2","function":{"name":"read_file","arguments":"{}"}}]}}]}
data: [DONE]
"#;
        let resp = feed_all(sse);
        match resp.message {
            Message::Assistant { content, tool_calls } => {
                assert!(content.is_none());
                assert_eq!(tool_calls.len(), 2);
                assert_eq!(tool_calls[0].id, "call_1");
                assert_eq!(tool_calls[0].name, "write_file");
                assert_eq!(tool_calls[0].arguments, r#"{"path":"a.md"}"#);
                assert_eq!(tool_calls[1].name, "read_file");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn stream_error_chunk_is_an_api_error() {
        let mut acc = StreamAcc::default();
        let err = acc
            .feed_line(r#"data: {"error":{"message":"overloaded"}}"#)
            .unwrap_err();
        assert!(err.to_string().contains("overloaded"), "{err}");
    }

    #[test]
    fn build_request_basic() {
        let req = ChatRequest {
            messages: vec![
                Message::System("You are helpful.".into()),
                Message::User("Hello".into()),
            ],
            tools: vec![],
            temperature: Some(0.7),
            max_tokens: Some(1024),
        };
        let oai = build_request("gpt-4o", &req);
        assert_eq!(oai.model, "gpt-4o");
        assert_eq!(oai.messages.len(), 2);
        assert_eq!(oai.messages[0].role, "system");
        assert_eq!(oai.messages[1].role, "user");
        assert!(oai.tools.is_empty());
        assert_eq!(oai.temperature, Some(0.7));
    }

    #[test]
    fn build_request_with_tools() {
        let req = ChatRequest {
            messages: vec![Message::User("Search for rust".into())],
            tools: vec![ToolDefinition {
                name: "web_search".into(),
                description: "Search the web".into(),
                parameters: serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}}),
            }],
            temperature: None,
            max_tokens: None,
        };
        let oai = build_request("gpt-4o", &req);
        assert_eq!(oai.tools.len(), 1);
        assert_eq!(oai.tools[0].r#type, "function");
        assert_eq!(oai.tools[0].function.name, "web_search");
    }

    #[test]
    fn message_conversion_assistant_with_tool_calls() {
        let msg = Message::Assistant {
            content: Some("Let me search.".into()),
            tool_calls: vec![ToolCallInfo {
                id: "tc-1".into(),
                name: "web_search".into(),
                arguments: r#"{"query":"rust"}"#.into(),
            }],
        };
        let oai = message_to_oai(&msg);
        assert_eq!(oai.role, "assistant");
        assert_eq!(oai.content.as_deref(), Some("Let me search."));
        let tcs = oai.tool_calls.unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].function.name, "web_search");
    }

    #[test]
    fn message_conversion_tool_result() {
        let msg = Message::ToolResult {
            tool_call_id: "tc-1".into(),
            content: "Found 10 results".into(),
        };
        let oai = message_to_oai(&msg);
        assert_eq!(oai.role, "tool");
        assert_eq!(oai.tool_call_id.as_deref(), Some("tc-1"));
        assert_eq!(oai.content.as_deref(), Some("Found 10 results"));
    }

    #[test]
    fn parse_response_text_only() {
        let json = serde_json::json!({
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                }
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });
        let oai_resp: OaiResponse = serde_json::from_value(json).unwrap();
        let resp = parse_response(oai_resp).unwrap();
        assert_eq!(resp.model, "gpt-4o");
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
        match &resp.message {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content.as_deref(), Some("Hello!"));
                assert!(tool_calls.is_empty());
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn parse_response_cache_telemetry() {
        let json = serde_json::json!({
            "model": "gpt-4o",
            "choices": [{
                "message": {"role": "assistant", "content": "Hi"}
            }],
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 5,
                "prompt_tokens_details": {"cached_tokens": 750}
            }
        });
        let oai_resp: OaiResponse = serde_json::from_value(json).unwrap();
        let resp = parse_response(oai_resp).unwrap();
        // prompt_tokens already includes cached tokens — no normalisation.
        assert_eq!(resp.usage.input_tokens, 1000);
        assert_eq!(resp.usage.cached_input_tokens, 750);
        assert_eq!(resp.usage.cache_creation_tokens, 0);
        assert!((resp.usage.cache_hit_rate() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn parse_response_with_tool_calls() {
        let json = serde_json::json!({
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "web_search",
                            "arguments": "{\"query\":\"rust\"}"
                        }
                    }]
                }
            }],
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 15
            }
        });
        let oai_resp: OaiResponse = serde_json::from_value(json).unwrap();
        let resp = parse_response(oai_resp).unwrap();
        match &resp.message {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                assert!(content.is_none());
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "web_search");
                assert_eq!(tool_calls[0].id, "call_123");
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn parse_response_empty_choices() {
        let json = serde_json::json!({
            "model": "gpt-4o",
            "choices": [],
            "usage": { "prompt_tokens": 0, "completion_tokens": 0 }
        });
        let oai_resp: OaiResponse = serde_json::from_value(json).unwrap();
        let err = parse_response(oai_resp).unwrap_err();
        assert!(matches!(err, LlmError::Serialization(_)));
    }

    #[test]
    fn request_serialization_roundtrip() {
        let req = ChatRequest {
            messages: vec![Message::System("sys".into()), Message::User("hi".into())],
            tools: vec![ToolDefinition {
                name: "t".into(),
                description: "d".into(),
                parameters: serde_json::json!({}),
            }],
            temperature: Some(0.5),
            max_tokens: Some(100),
        };
        let oai = build_request("model", &req);
        let json = serde_json::to_string(&oai).unwrap();
        // Verify it's valid JSON that can round-trip.
        let _: serde_json::Value = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn legacy_model_uses_max_tokens() {
        let req = ChatRequest {
            messages: vec![Message::User("hi".into())],
            tools: vec![],
            temperature: None,
            max_tokens: Some(1024),
        };
        let oai = build_request("gpt-4o", &req);
        assert_eq!(oai.max_tokens, Some(1024));
        assert_eq!(oai.max_completion_tokens, None);
    }

    #[test]
    fn reasoning_model_uses_max_completion_tokens_and_drops_temperature() {
        for model in [
            "gpt-5-mini",
            "gpt-5",
            "o1-preview",
            "o3-mini",
            "gpt-4.5-preview",
        ] {
            let req = ChatRequest {
                messages: vec![Message::User("hi".into())],
                tools: vec![],
                temperature: Some(0.7),
                max_tokens: Some(2048),
            };
            let oai = build_request(model, &req);
            assert_eq!(oai.max_tokens, None, "{model}: should not send max_tokens");
            assert_eq!(
                oai.max_completion_tokens,
                Some(2048),
                "{model}: should send max_completion_tokens"
            );
            assert_eq!(oai.temperature, None, "{model}: should drop temperature");
        }
    }

    #[test]
    fn reasoning_model_omits_all_when_none() {
        let req = ChatRequest {
            messages: vec![Message::User("hi".into())],
            tools: vec![],
            temperature: None,
            max_tokens: None,
        };
        let oai = build_request("gpt-5-mini", &req);
        let json = serde_json::to_string(&oai).unwrap();
        assert!(!json.contains("max_tokens"));
        assert!(!json.contains("max_completion_tokens"));
        assert!(!json.contains("temperature"));
    }
}
