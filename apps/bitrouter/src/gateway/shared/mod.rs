//! Shared gateway authority. Local operation never initializes this backend.

mod authentication;
mod authority;
pub(crate) mod credentials;
mod crypto;
mod engine;
mod publication;
mod schema;
mod store;

#[cfg(test)]
mod tests;
