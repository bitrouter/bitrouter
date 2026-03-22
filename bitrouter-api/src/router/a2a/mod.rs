//! A2A v0.3.0 gateway routes.
//!
//! Provides Warp filters that proxy A2A protocol operations
//! to upstream agents via the [`UpstreamAgentRegistry`](bitrouter_a2a::client::registry::UpstreamAgentRegistry).

mod convert;
mod discovery;
pub mod filters;
mod messaging;
mod observe;
mod push;
mod tasks;
#[cfg(test)]
mod tests;
pub mod types;

pub use filters::a2a_gateway_filter;
