//! # bitrouter-auth
//!
//! Auth plugin. Provides [`AuthHook`] — a `language_model::PreRequestHook` that
//! validates `brvk_` virtual keys (and, later, MPP credentials) — owns the
//! `users` and `api_keys` tables, and emits the [`Authenticated`] event. v1 has
//! **no JWT path** (004 §3.0).
//!
//! This plugin owns its tables exclusively; other plugins coordinate via the
//! [`Authenticated`] event, never by reading `api_keys` directly.

#![forbid(unsafe_code)]

pub mod db;
pub mod events;
pub mod hook;
pub mod keys;

#[cfg(test)]
mod tests;

pub use db::{ApiKeyRecord, NewApiKey, migrate, migrations};
pub use events::{Authenticated, MppVerified};
pub use hook::{AuthHook, plugin_id};
pub use keys::{GeneratedKey, KEY_PREFIX, generate, hash_key, looks_like_virtual_key};
