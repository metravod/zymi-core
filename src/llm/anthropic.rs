use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::types::{Message, TokenUsage, ToolCallInfo};

use super::error::LlmError;
use super::{ChatRequest, ChatResponse, LlmProvider};

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic Messages API provider.
#[derive(Debug)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl AnthropicProvider {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            api_key,
            model,
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat_completion(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let anth_request = build_request(&self.model, request);
        let url = format!("{}/messages", self.base_url);

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&anth_request)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status,
                message: body,
            });
        }

        let anth_resp: AnthResponse = resp
            .json()
            .await
            .map_err(|e| LlmError::Serialization(e.to_string()))?;

        parse_response(anth_resp)
    }
}

// ---------------------------------------------------------------------------
// Wire types — Anthropic Messages API
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct AnthRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AnthMessage {
    role: String,
    content: AnthContent,
}

/// Content can be a plain string or an array of content blocks.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum AnthContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Debug, Serialize)]
struct AnthTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct AnthResponse {
    model: String,
    content: Vec<ContentBlock>,
    usage: AnthUsage,
}

#[derive(Debug, Deserialize)]
struct AnthUsage {
    input_tokens: u32,
    output_tokens: u32,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn build_request(model: &str, request: &ChatRequest) -> AnthRequest {
    let mut system = None;
    let mut messages = Vec::new();

    for msg in &request.messages {
        match msg {
            Message::System(text) => {
                system = Some(text.clone());
            }
            Message::User(text) => {
                messages.push(AnthMessage {
                    role: "user".into(),
                    content: AnthContent::Text(text.clone()),
                });
            }
            Message::UserMultimodal { parts } => {
                let text = parts
                    .iter()
                    .filter_map(|p| match p {
                        crate::types::ContentPart::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.push(AnthMessage {
                    role: "user".into(),
                    content: AnthContent::Text(text),
                });
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks = Vec::new();
                if let Some(text) = content {
                    blocks.push(ContentBlock::Text { text: text.clone() });
                }
                for tc in tool_calls {
                    let input: serde_json::Value =
                        serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Object(
                            serde_json::Map::new(),
                        ));
                    blocks.push(ContentBlock::ToolUse {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        input,
                    });
                }
                messages.push(AnthMessage {
                    role: "assistant".into(),
                    content: AnthContent::Blocks(blocks),
                });
            }
            Message::ToolResult {
                tool_call_id,
                content,
            } => {
                messages.push(AnthMessage {
                    role: "user".into(),
                    content: AnthContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: tool_call_id.clone(),
                        content: content.clone(),
                    }]),
                });
            }
        }
    }

    let tools = request
        .tools
        .iter()
        .map(|t| AnthTool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.parameters.clone(),
        })
        .collect();

    AnthRequest {
        model: model.into(),
        max_tokens: request.max_tokens.unwrap_or(4096),
        system,
        messages,
        tools,
        temperature: request.temperature,
    }
}

fn parse_response(resp: AnthResponse) -> Result<ChatResponse, LlmError> {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for block in resp.content {
        match block {
            ContentBlock::Text { text } => text_parts.push(text),
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCallInfo {
                    id,
                    name,
                    arguments: serde_json::to_string(&input)
                        .unwrap_or_else(|_| "{}".into()),
                });
            }
            ContentBlock::ToolResult { .. } => {}
        }
    }

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };

    let message = Message::Assistant {
        content,
        tool_calls,
    };

    let usage = TokenUsage {
        input_tokens: resp.usage.input_tokens,
        output_tokens: resp.usage.output_tokens,
    };

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
    fn build_request_extracts_system() {
        let req = ChatRequest {
            messages: vec![
                Message::System("You are helpful.".into()),
                Message::User("Hello".into()),
            ],
            tools: vec![],
            temperature: None,
            max_tokens: None,
        };
        let anth = build_request("claude-sonnet-4-20250514", &req);
        assert_eq!(anth.system.as_deref(), Some("You are helpful."));
        assert_eq!(anth.messages.len(), 1);
        assert_eq!(anth.messages[0].role, "user");
    }

    #[test]
    fn build_request_default_max_tokens() {
        let req = ChatRequest {
            messages: vec![Message::User("Hi".into())],
            tools: vec![],
            temperature: None,
            max_tokens: None,
        };
        let anth = build_request("model", &req);
        assert_eq!(anth.max_tokens, 4096);
    }

    #[test]
    fn build_request_custom_max_tokens() {
        let req = ChatRequest {
            messages: vec![Message::User("Hi".into())],
            tools: vec![],
            temperature: None,
            max_tokens: Some(1024),
        };
        let anth = build_request("model", &req);
        assert_eq!(anth.max_tokens, 1024);
    }

    #[test]
    fn build_request_with_tools() {
        let req = ChatRequest {
            messages: vec![Message::User("Search".into())],
            tools: vec![ToolDefinition {
                name: "web_search".into(),
                description: "Search the web".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            temperature: None,
            max_tokens: None,
        };
        let anth = build_request("model", &req);
        assert_eq!(anth.tools.len(), 1);
        assert_eq!(anth.tools[0].name, "web_search");
    }

    #[test]
    fn build_request_assistant_with_tool_calls() {
        let req = ChatRequest {
            messages: vec![
                Message::User("Search rust".into()),
                Message::Assistant {
                    content: Some("Let me search.".into()),
                    tool_calls: vec![ToolCallInfo {
                        id: "tu-1".into(),
                        name: "web_search".into(),
                        arguments: r#"{"query":"rust"}"#.into(),
                    }],
                },
                Message::ToolResult {
                    tool_call_id: "tu-1".into(),
                    content: "Found results".into(),
                },
            ],
            tools: vec![],
            temperature: None,
            max_tokens: None,
        };
        let anth = build_request("model", &req);
        assert_eq!(anth.messages.len(), 3);
        // Assistant message should have blocks.
        match &anth.messages[1].content {
            AnthContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2); // text + tool_use
            }
            _ => panic!("expected blocks"),
        }
        // Tool result should be a user message with tool_result block.
        assert_eq!(anth.messages[2].role, "user");
    }

    #[test]
    fn parse_response_text_only() {
        let json = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "content": [
                {"type": "text", "text": "Hello!"}
            ],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5
            }
        });
        let anth_resp: AnthResponse = serde_json::from_value(json).unwrap();
        let resp = parse_response(anth_resp).unwrap();
        assert_eq!(resp.model, "claude-sonnet-4-20250514");
        assert_eq!(resp.usage.input_tokens, 10);
        match &resp.message {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content.as_deref(), Some("Hello!"));
                assert!(tool_calls.is_empty());
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn parse_response_with_tool_use() {
        let json = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "content": [
                {"type": "text", "text": "Let me search."},
                {
                    "type": "tool_use",
                    "id": "tu_123",
                    "name": "web_search",
                    "input": {"query": "rust"}
                }
            ],
            "usage": {
                "input_tokens": 20,
                "output_tokens": 15
            }
        });
        let anth_resp: AnthResponse = serde_json::from_value(json).unwrap();
        let resp = parse_response(anth_resp).unwrap();
        match &resp.message {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content.as_deref(), Some("Let me search."));
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].id, "tu_123");
                assert_eq!(tool_calls[0].name, "web_search");
                assert!(tool_calls[0].arguments.contains("rust"));
            }
            _ => panic!("expected Assistant"),
        }
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
            max_tokens: Some(2048),
        };
        let anth = build_request("model", &req);
        let json = serde_json::to_string(&anth).unwrap();
        let _: serde_json::Value = serde_json::from_str(&json).unwrap();
    }
}
