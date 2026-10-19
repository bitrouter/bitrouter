//! Shared gateway authority. Local operation never initializes this backend.

mod authority;
mod credentials;
mod crypto;
mod engine;
mod publication;
mod schema;
mod store;

#[cfg(test)]
mod tests;
