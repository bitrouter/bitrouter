//! # bitrouter-guardrails
//!
//! Content firewall plugin. Provides [`GuardrailPreHook`] (upstream / request
//! content — denies on a `Block` rule) and [`GuardrailStreamHook`] (downstream
//! / response stream — redacts `Redact` matches, aborts on `Block`). See
//! design doc 004 §5.

#![forbid(unsafe_code)]

pub mod hooks;
pub mod rules;

#[cfg(test)]
mod tests;

pub use hooks::{GuardrailPreHook, GuardrailStreamHook};
pub use rules::{Action, GuardrailRule, REDACTION, RuleSet, SlidingWindowMatcher, WindowResult};
