//! Hosted request authentication over explicit origin-bound credential transactions.

pub mod applier;
pub mod credentials;
pub mod session;

pub mod metadata;
pub mod tokens;

#[cfg(feature = "hosted-login")]
pub mod flow;

/// Hosted provider identifier.
pub const PROVIDER_ID: &str = "bitrouter";
