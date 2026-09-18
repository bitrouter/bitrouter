//! # bitrouter-guardrails
//!
//! Guardrail configuration and matching with an optional trusted BitRouter SDK
//! adapter. The default build contains only the data contract and matcher, so
//! independent checker services do not link the BitRouter host runtime.
//!
//! With the `sdk` feature, the compatibility hooks read the active [`RuleSet`]
//! from the pipeline's typed extensions,
//! so the rule set can be either a fixed global set or one resolved per request
//! by the host. `GuardrailsPlugin` wires the hooks into an
//! `bitrouter_sdk::AppBuilder` in one call: `GuardrailsPlugin::with_static`
//! for the global case (it also installs a `DepositRulesHook`), or
//! `GuardrailsPlugin::dynamic` when the host deposits a per-request rule set
//! itself. [`GuardrailConfig`] is the serializable data contract a host loads
//! from any source and compiles into a [`RuleSet`]. See design doc.

#![forbid(unsafe_code)]

pub mod config;
#[cfg(feature = "sdk")]
pub mod hooks;
#[cfg(feature = "sdk")]
pub mod plugin;
pub mod rules;

#[cfg(all(test, feature = "sdk"))]
mod tests;

pub use config::{GuardrailConfig, RuleSpec};
#[cfg(feature = "sdk")]
pub use hooks::{DepositRulesHook, GuardrailPreHook, GuardrailStreamHook};
#[cfg(feature = "sdk")]
pub use plugin::GuardrailsPlugin;
pub use rules::{Action, GuardrailRule, REDACTION, RuleSet, SlidingWindowMatcher, WindowResult};
