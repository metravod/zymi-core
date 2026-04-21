//! Model Context Protocol (MCP) client.
//!
//! v1 scope (per ADR-0023): tools capability only, stdio transport with
//! newline-delimited JSON framing. Resources, prompts, sampling, and HTTP+SSE
//! transport are out of scope until follow-up ADRs.
//!
//! The underlying JSON-RPC 2.0 transport lives in `crate::plugin::transport`
//! (extracted in P2 so the external-process plugin protocol can reuse it).

pub mod connection;
pub mod registry;

pub use crate::plugin::transport::{Transport, TransportError};
pub use connection::{McpCallResult, McpError, McpServerConnection, McpServerSpec, McpTool};
pub use registry::{make_id, parse_id, validate_segment, McpRegistry, RestartPolicy, MCP_PREFIX};
