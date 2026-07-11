//! Browser-based PKCE OAuth + RFC 8628 Device Code flow + an on-disk
//! credential store shared by both.
//!
//! Bitrouter's interactive logins all land in
//! [`credential_store::CredentialStore`], regardless of which flow produced
//! them — the on-disk file is keyed by `(provider_id, label)` and can hold
//! either an OAuth credential (with refresh metadata) or a static API key
//! the user pasted at `bitrouter providers login`. Provider-specific layers
//! (`crate::anthropic`, `crate::codex`, `crate::copilot`) read this store
//! to drive each request's `AuthApplier`.
//!
//! Module map:
//!
//! - [`credential_store`] — the persistent JSON store. Public unconditionally
//!   because non-pkce builds still need it for the device-code flow.
//! - [`device_code`] — RFC 8628 device authorization grant. Drives the
//!   `bitrouter providers login github-copilot` path.
//! - [`pkce`], [`auth_code`], [`listener`], [`refresh`], [`registry`] —
//!   the browser-based PKCE Authorization Code flow + loopback redirect
//!   listener + `refresh_token` grant. Gated behind the `pkce` Cargo
//!   feature so the `sha2` / `base64` / `rand` / `url` deps only land when
//!   the feature is enabled.

pub mod credential_store;
pub mod device_code;

#[cfg(feature = "pkce")]
pub mod auth_code;
#[cfg(feature = "pkce")]
pub mod listener;
#[cfg(feature = "pkce")]
pub mod login;
#[cfg(feature = "pkce")]
pub mod pkce;
#[cfg(feature = "pkce")]
pub mod refresh;
#[cfg(feature = "pkce")]
pub mod registry;

// Legacy re-exports — predate the credential-store rename. Kept so existing
// `use bitrouter_providers::oauth::{DeviceCodeFlow, OAuthToken, …}` paths
// keep compiling. New code should reach into the submodule directly.
pub use credential_store::OAuthToken;
pub use device_code::{DeviceCodeFlow, DeviceCodeParams, DeviceCodeResponse, FlowError, FlowEvent};
