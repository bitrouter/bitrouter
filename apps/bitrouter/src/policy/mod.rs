//! OSS policy engine — per-API-key model allow/deny, spend ceiling,
//! expiry, tool-access rules, request-rate limits.
//!
//! Not a shared library plugin: the OSS binary owns this implementation
//! end-to-end. A closed cloud product writes its own `PreRequestHook`
//! against its own policy model. The OSS engine reads spend / rate data
//! directly from the sibling [`crate::metering::MeteringStore`] — no
//! `MetricsStore` trait between them, just a concrete type call.

pub mod hook;
#[allow(clippy::module_inception)]
pub mod policy;
pub mod store;

#[cfg(test)]
mod tests;

pub use hook::PolicyHook;
pub use policy::{EffectivePolicy, Policy, PolicyViolation};
pub use store::PolicyStore;
