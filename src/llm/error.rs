use thiserror::Error;

/// `err` and every `source()` below it, joined with ": ".
pub(crate) fn source_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(e) = cur {
        let s = e.to_string();
        if !out.contains(&s) {
            out.push_str(": ");
            out.push_str(&s);
        }
        cur = e.source();
    }
    out
}

#[derive(Debug, Error)]
pub enum LlmError {
    /// reqwest's own Display stops at "error sending request for url (…)";
    /// the actual cause (timeout, reset, TLS, DNS) lives in the source
    /// chain, so print the whole chain.
    #[error("HTTP request failed: {}", source_chain(.0))]
    Http(#[from] reqwest::Error),

    /// The connection died after a long wait with no response — the
    /// signature of a gateway cutting idle requests, not of a dead endpoint.
    #[error(
        "{url}: connection closed after {elapsed_secs}s without a response ({cause}). \
         A gateway in front of the model is likely cutting long requests; \
         set `stream: true` on this provider so tokens flow while it generates"
    )]
    Dropped {
        url: String,
        elapsed_secs: u64,
        cause: String,
    },

    #[error("API error (status {status}): {message}")]
    Api { status: u16, message: String },

    #[error("invalid LLM config: {0}")]
    InvalidConfig(String),

    #[error("serialization error: {0}")]
    Serialization(String),
}
