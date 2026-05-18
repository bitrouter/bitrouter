//! Runtime-side authentication helpers.
//!
//! The OAuth token store is read by the router whenever a provider uses
//! `auth.type: oauth` so refreshed access tokens transparently take effect.
//! The interactive OAuth login flow itself lives in `bitrouter-cli` — this
//! crate only persists and reads the resulting tokens.

pub mod token_store;
