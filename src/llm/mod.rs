pub mod anthropic;
pub mod error;
pub mod openai;

use async_trait::async_trait;

use crate::config::LlmConfig;
use crate::types::{Message, TokenUsage, ToolDefinition};

pub use error::LlmError;

/// Request for an LLM chat completion.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    #[allow(clippy::struct_field_names)]
    pub tools: Vec<ToolDefinition>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

/// Response from an LLM chat completion.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// The assistant's reply (always `Message::Assistant`).
    pub message: Message,
    pub usage: TokenUsage,
    pub model: String,
}

/// Unified interface for LLM providers.
///
/// Implementations handle wire-format conversion and HTTP transport.
/// The trait is object-safe so providers can be stored as `Box<dyn LlmProvider>`.
#[async_trait]
pub trait LlmProvider: Send + Sync + std::fmt::Debug {
    async fn chat_completion(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError>;
}

/// Create an LLM provider from a project config.
///
/// Recognized providers: `openai`, `anthropic`, `ollama`, `vllm`, `together`.
/// OpenAI-compatible providers (ollama, vllm, together) use [`openai::OpenAiProvider`]
/// with appropriate default base URLs.
pub fn create_provider(config: &LlmConfig) -> Result<Box<dyn LlmProvider>, LlmError> {
    match config.provider.as_str() {
        "openai" => {
            let base_url = config
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".into());
            Ok(Box::new(openai::OpenAiProvider::new(
                base_url,
                config.api_key.clone(),
                config.model.clone(),
            )))
        }
        "ollama" => {
            let base_url = config
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434/v1".into());
            Ok(Box::new(openai::OpenAiProvider::new(
                base_url,
                config.api_key.clone(),
                config.model.clone(),
            )))
        }
        "vllm" => {
            let base_url = config
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:8000/v1".into());
            Ok(Box::new(openai::OpenAiProvider::new(
                base_url,
                config.api_key.clone(),
                config.model.clone(),
            )))
        }
        "together" => {
            let base_url = config
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.together.xyz/v1".into());
            Ok(Box::new(openai::OpenAiProvider::new(
                base_url,
                config.api_key.clone(),
                config.model.clone(),
            )))
        }
        "anthropic" => {
            let api_key = config.api_key.clone().ok_or_else(|| {
                LlmError::InvalidConfig("Anthropic provider requires an api_key".into())
            })?;
            let base_url = config
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.anthropic.com/v1".into());
            Ok(Box::new(anthropic::AnthropicProvider::new(
                base_url,
                api_key,
                config.model.clone(),
            )))
        }
        other => Err(LlmError::InvalidConfig(format!(
            "unknown provider `{other}` — expected one of: openai, anthropic, ollama, vllm, together"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(provider: &str) -> LlmConfig {
        LlmConfig {
            provider: provider.into(),
            base_url: None,
            model: "test-model".into(),
            api_key: Some("sk-test".into()),
        }
    }

    #[test]
    fn factory_openai() {
        let p = create_provider(&config("openai"));
        assert!(p.is_ok());
    }

    #[test]
    fn factory_anthropic() {
        let p = create_provider(&config("anthropic"));
        assert!(p.is_ok());
    }

    #[test]
    fn factory_ollama() {
        let p = create_provider(&config("ollama"));
        assert!(p.is_ok());
    }

    #[test]
    fn factory_vllm() {
        let p = create_provider(&config("vllm"));
        assert!(p.is_ok());
    }

    #[test]
    fn factory_together() {
        let p = create_provider(&config("together"));
        assert!(p.is_ok());
    }

    #[test]
    fn factory_unknown_provider() {
        let err = create_provider(&config("deepseek")).unwrap_err();
        assert!(matches!(err, LlmError::InvalidConfig(_)));
        assert!(err.to_string().contains("deepseek"));
    }

    #[test]
    fn factory_anthropic_requires_api_key() {
        let cfg = LlmConfig {
            provider: "anthropic".into(),
            base_url: None,
            model: "claude-sonnet-4-20250514".into(),
            api_key: None,
        };
        let err = create_provider(&cfg).unwrap_err();
        assert!(matches!(err, LlmError::InvalidConfig(_)));
        assert!(err.to_string().contains("api_key"));
    }

    #[test]
    fn factory_openai_no_key_ok() {
        let cfg = LlmConfig {
            provider: "openai".into(),
            base_url: Some("http://localhost:1234/v1".into()),
            model: "local-model".into(),
            api_key: None,
        };
        assert!(create_provider(&cfg).is_ok());
    }
}
