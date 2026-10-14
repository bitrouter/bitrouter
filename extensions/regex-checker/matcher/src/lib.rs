//! Compiled regex rules and request-check callbacks.
//!
//! Capability inputs and decisions come from `bitrouter-ai`. Hosts own
//! registration and execution. Legacy global/output hooks live in
//! `bitrouter::gateway::extensions::guardrails` under its `guardrails` feature.

#![forbid(unsafe_code)]

pub mod checker;
pub mod config;
pub mod rules;
