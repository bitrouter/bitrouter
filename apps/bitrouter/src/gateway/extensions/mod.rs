//! Application-owned capability activation and host adapters.

#[cfg(feature = "guardrails")]
pub mod guardrails;

pub mod plugin;
pub mod registry;

/// Native request-check inputs, decisions and callback contract.
pub mod request_check;
