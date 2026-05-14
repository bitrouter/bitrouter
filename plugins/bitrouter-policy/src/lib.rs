//! # bitrouter-policy
//!
//! Policy plugin. Provides [`PolicyHook`] — a `language_model::PreRequestHook`
//! enforcing per-API-key model allow/deny lists, spend ceilings and expiry,
//! with **explicit combination semantics** (004 §4.2) when several policies
//! apply at once. Policies are pure file config; this plugin owns no tables.

#![forbid(unsafe_code)]

pub mod hook;
pub mod policy;
pub mod store;

#[cfg(test)]
mod tests;

pub use hook::PolicyHook;
pub use policy::{EffectivePolicy, Policy, PolicyViolation};
pub use store::PolicyStore;
