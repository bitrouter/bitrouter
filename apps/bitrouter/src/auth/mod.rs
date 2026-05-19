//! OSS auth — `brvk_` virtual-key authentication, the `Authenticated`
//! event, and the `users` + `api_keys` tables.
//!
//! Not a shared library plugin: the OSS binary owns this implementation
//! end-to-end. A closed cloud product writes its own `PreRequestHook`
//! against its own auth model.

pub mod db;
pub mod events;
pub mod hook;
pub mod keys;

#[cfg(test)]
mod tests;

pub use db::{ApiKeyRecord, NewApiKey, migrate};
pub use events::Authenticated;
pub use hook::AuthHook;
pub use keys::{GeneratedKey, KEY_PREFIX, generate, hash_key, looks_like_virtual_key};
