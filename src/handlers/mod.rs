//! Command handlers — slice 1 of the runtime unification (ADR-0013).
//!
//! Each submodule contains the implementation of one command from
//! [`crate::commands`]. Handlers take an `&Runtime` and a command, dispatch
//! against the runtime's wiring, and return a domain result.
//!
//! Slice 1 only ships [`run_pipeline`] — see ADR-0013 for the larger set.

pub mod run_pipeline;
