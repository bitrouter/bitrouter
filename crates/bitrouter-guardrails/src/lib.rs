//! # bitrouter-guardrails
//!
//! Content firewall plugin. Provides [`GuardrailPreHook`] (upstream / request
//! content — denies on a `Block` rule) and [`GuardrailStreamHook`] (downstream
//! / response stream — redacts `Redact` matches, aborts on `Block`).
//!
//! Both hooks read the active [`RuleSet`] from the pipeline's typed extensions,
//! so the rule set can be either a fixed global set or one resolved per request
//! by the host. [`GuardrailsPlugin`] wires the hooks into an
//! [`bitrouter_sdk::AppBuilder`] in one call: [`GuardrailsPlugin::with_static`]
//! for the global case (it also installs a [`DepositRulesHook`]), or
//! [`GuardrailsPlugin::dynamic`] when the host deposits a per-request rule set
//! itself. [`GuardrailConfig`] is the serializable data contract a host loads
//! from any source and compiles into a [`RuleSet`]. See design doc.

#![forbid(unsafe_code)]

pub mod config;
pub mod hooks;
pub mod plugin;
pub mod rules;

#[cfg(test)]
mod tests;

pub use config::{GuardrailConfig, RuleSpec};
pub use hooks::{DepositRulesHook, GuardrailPreHook, GuardrailStreamHook};
pub use plugin::GuardrailsPlugin;
pub use rules::{Action, GuardrailRule, REDACTION, RuleSet, SlidingWindowMatcher, WindowResult};
