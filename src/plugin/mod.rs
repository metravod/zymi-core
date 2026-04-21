//! Shared plumbing for pluggable components (MCP, external-process plugins,
//! future connector categories).
//!
//! Today this module owns the JSON-RPC 2.0 stdio transport extracted from the
//! MCP client (ADR-0023). It is framework-neutral: any subprocess protocol
//! that frames requests as one JSON object per line can build on top of it.
//! The external-process plugin protocol (ADR-0021) is the next planned
//! consumer.

pub mod registry;
pub mod transport;

pub use registry::{PluginBuildError, PluginBuilder, PluginRegistry};
