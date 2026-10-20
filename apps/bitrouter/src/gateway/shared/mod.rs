//! Shared gateway authority. Local operation never initializes this backend.

pub(crate) mod admitted;
mod authentication;
mod authority;
pub(crate) mod continuation;
pub(crate) mod credentials;
mod crypto;
pub(crate) mod dispatched;
mod engine;
mod http;
mod ownership;
mod publication;
mod schema;
mod store;
mod transport;

#[cfg(test)]
mod tests;
