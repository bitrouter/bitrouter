//! OSS auth — `brvk_` virtual-key authentication, the `Authenticated`
//! event, and the `users` + `api_keys` tables.
//!
//! Not a shared library plugin: the OSS binary owns this implementation
//! end-to-end. A closed cloud product writes its own `PreRequestHook`
//! against its own auth model.

pub mod db;
pub mod entities;
pub mod events;
pub mod hook;
pub mod keys;

#[cfg(test)]
mod tests;

pub use db::NewApiKey;
pub use hook::AuthHook;
pub use keys::generate;
