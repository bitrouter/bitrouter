//! A2A v1.0 gateway routes.
//!
//! Provides Warp filters that proxy A2A protocol operations
//! to an upstream agent via the [`A2aGateway`] trait.

mod convert;
pub mod filters;
#[cfg(test)]
mod tests;
pub mod types;

pub use filters::a2a_gateway_filter;
