//! Capability ports: the traits the orchestrator profile injects so the
//! substrate-coupled behavior (spawning ACP subagents, reading the metering
//! database) lives app-side while the crate keeps ownership of the tool
//! schemas and descriptions.
//!
//! Dependency inversion by design: this crate defines the traits in plain
//! `serde`/`serde_json` types, so it never grows a dependency on
//! `bitrouter-substrate`, `bitrouter-skills`, or `bitrouter-observe`. Each
//! port's result JSON is built adapter-side; the crate owns only the inputs.

pub mod cost;
pub mod fleet;
