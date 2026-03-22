//! A2A v0.3.0 gateway routes.
//!
//! Provides Warp filters that proxy A2A protocol operations
//! to an upstream agent via the [`A2aGateway`](types::A2aGateway) trait.

mod convert;
mod discovery;
pub mod filters;
mod messaging;
mod push;
mod rest;
mod tasks;
#[cfg(test)]
mod tests;
pub mod types;

pub use filters::a2a_gateway_filter;
