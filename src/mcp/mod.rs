//! Model Context Protocol (MCP) client.
//!
//! v1 scope (per ADR-0023): tools capability only, stdio transport with
//! newline-delimited JSON framing. Resources, prompts, sampling, and HTTP+SSE
//! transport are out of scope until follow-up ADRs.

pub mod connection;
pub mod registry;
pub mod transport;

pub use connection::{McpCallResult, McpError, McpServerConnection, McpServerSpec, McpTool};
pub use registry::{make_id, parse_id, validate_segment, McpRegistry, MCP_PREFIX};
pub use transport::{Transport, TransportError};
