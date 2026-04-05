use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API error (status {status}): {message}")]
    Api { status: u16, message: String },

    #[error("invalid LLM config: {0}")]
    InvalidConfig(String),

    #[error("serialization error: {0}")]
    Serialization(String),
}
