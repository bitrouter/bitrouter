//! # bitrouter-cloud-sdk
//!
//! Client SDK for the BitRouter Cloud control plane.
//!
//! ## Scope
//!
//! Today this crate ships three pieces of functionality:
//!
//! 1. [`auth`] — OAuth 2.0 sign-in for the CLI against the BitRouter Cloud
//!    authorization server. Implements RFC 8628 device-flow login,
//!    RFC 6749 §6 refresh, and RFC 7009 best-effort revocation, plus an
//!    on-disk credentials store (mode `0o600` on Unix). The
//!    `bitrouter auth login` / `logout` / `whoami` subcommands wire to the
//!    entry points in [`auth::commands`].
//! 2. [`provider`] — a [`bitrouter_sdk::language_model::AuthApplier`]
//!    implementation for the `bitrouter` provider (the official hosted
//!    gateway whose user-facing addressing is `bitrouter:<model-id>`).
//!    Prefers an OAuth
//!    access token from the credentials store (auto-refreshed within
//!    [`auth::credentials::REFRESH_WINDOW`] of expiry); falls back to a
//!    static `brk_…` API key carried on the routing target; otherwise
//!    returns a 401 with onboarding guidance.
//! 3. [`management`] — a typed HTTP client for the BitRouter Cloud
//!    `/v1/*` management surface (`keys`, `usage`, `billing`, `policies`,
//!    `budgets`, `presets`, `byok`, `oauth_clients`). Shares the auth
//!    module's credentials store, so it transparently picks up the
//!    bearer that `bitrouter auth login` persisted and refreshes it on
//!    the same RFC 6749 §6 / §4.14 schedule. Drives the
//!    `bitrouter cloud` CLI subcommands.
//!
//! ## Authoritative references
//!
//! - RFC 6749 — The OAuth 2.0 Authorization Framework: <https://www.rfc-editor.org/rfc/rfc6749>
//! - RFC 6750 — Bearer Token Usage: <https://www.rfc-editor.org/rfc/rfc6750>
//! - RFC 7009 — Token Revocation: <https://www.rfc-editor.org/rfc/rfc7009>
//! - RFC 8414 — Authorization Server Metadata: <https://www.rfc-editor.org/rfc/rfc8414>
//! - RFC 8628 — Device Authorization Grant: <https://www.rfc-editor.org/rfc/rfc8628>
//! - RFC 9700 — OAuth 2.0 Security Best Current Practice: <https://www.rfc-editor.org/rfc/rfc9700>

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod auth;
pub mod management;
pub mod provider;

pub use provider::BitrouterCloudAuthApplier;
