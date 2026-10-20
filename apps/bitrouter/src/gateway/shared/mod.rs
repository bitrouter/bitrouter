//! Shared gateway authority. Local operation never initializes this backend.

pub(crate) mod admitted;
mod authentication;
mod authority;
pub(crate) mod credentials;
mod crypto;
pub(crate) mod dispatched;
mod engine;
mod publication;
mod schema;
mod store;

#[cfg(test)]
mod tests;
