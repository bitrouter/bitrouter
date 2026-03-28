//! A2A v0.3.0 gateway routes.
//!
//! Provides Warp filters that proxy A2A protocol operations
//! to upstream agents via any [`A2aGateway`](bitrouter_core::api::a2a::gateway::A2aGateway) implementation.

mod filters;
#[cfg(test)]
mod tests;

pub use filters::a2a_gateway_filter;
