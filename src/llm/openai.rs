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
}

impl OpenAiProvider {
    pub fn new(base_url: String, api_key: Option<String>, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            api_key,
            model,
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat_completion(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let oai_request = build_request(&self.model, request);
        let url = format!("{}/chat/completions", self.base_url);

        let mut http = self.client.post(&url);
        if let Some(key) = &self.api_key {
            http = http.bearer_auth(key);
        }

        let resp = http.json(&oai_request).send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status,
                message: body,
            });
        }

        let oai_resp: OaiResponse = resp
            .json()
            .await
            .map_err(|e| LlmError::Serialization(e.to_string()))?;

        parse_response(oai_resp)
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
}

#[derive(Debug, Deserialize)]
struct OaiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

/// Newer OpenAI models (reasoning family) reject `max_tokens` (use
/// `max_completion_tokens`) and do not accept custom `temperature`.
fn is_reasoning_model(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("gpt-5")
        || m.starts_with("gpt-4.5")
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
    let temperature = if reasoning {
        None
    } else {
        request.temperature
    };

    OaiRequest {
        model: model.into(),
        messages,
        tools,
        temperature,
        max_tokens,
        max_completion_tokens,
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
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        })
        .unwrap_or_default();

    Ok(ChatResponse {
        message,
        usage,
        model: resp.model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolDefinition;

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
            messages: vec![
                Message::System("sys".into()),
                Message::User("hi".into()),
            ],
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
        for model in ["gpt-5-mini", "gpt-5", "o1-preview", "o3-mini", "gpt-4.5-preview"] {
            let req = ChatRequest {
                messages: vec![Message::User("hi".into())],
                tools: vec![],
                temperature: Some(0.7),
                max_tokens: Some(2048),
            };
            let oai = build_request(model, &req);
            assert_eq!(oai.max_tokens, None, "{model}: should not send max_tokens");
            assert_eq!(oai.max_completion_tokens, Some(2048), "{model}: should send max_completion_tokens");
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
