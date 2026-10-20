//! Shared gateway authority. Local operation never initializes this backend.

pub(crate) mod admitted;
mod authentication;
mod authority;
pub(crate) mod continuation;
pub(crate) mod credentials;
mod crypto;
pub(crate) mod dispatched;
mod engine;
mod generations;
mod http;
mod ownership;
mod publication;
pub(crate) mod runtime;
mod schema;
mod store;
pub(crate) mod telemetry;
mod transport;

#[cfg(test)]
mod tests;
