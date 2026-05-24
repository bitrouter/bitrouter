//! Sign-in to an OAuth 2.0 Authorization Server via the Device Authorization
//! Grant (RFC 8628). The "account" namespace is distinct from the local
//! `apps/bitrouter/src/auth/` module — that one mints local `brvk_` virtual
//! keys for callers of the bitrouter HTTP API. This one signs the CLI *into*
//! a user account on a remote AS.
//!
//! Central references:
//! - RFC 8628 — Device Authorization Grant: <https://www.rfc-editor.org/rfc/rfc8628>
//! - RFC 6749 — The OAuth 2.0 Authorization Framework: <https://www.rfc-editor.org/rfc/rfc6749>
//! - RFC 6750 — Bearer Token Usage: <https://www.rfc-editor.org/rfc/rfc6750>
//! - RFC 8414 — Authorization Server Metadata: <https://www.rfc-editor.org/rfc/rfc8414>
//! - RFC 7009 — Token Revocation: <https://www.rfc-editor.org/rfc/rfc7009>
//! - RFC 9700 — OAuth 2.0 Security Best Current Practice: <https://www.rfc-editor.org/rfc/rfc9700>
//!
//! ## Layout
//!
//! - [`settings`] — resolves the AS URL, client id and scope from CLI
//!   flag → env var → defaults.
//! - [`metadata`] — fetches and parses RFC 8414 AS metadata.
//! - [`flow`] — drives the RFC 8628 device flow and the RFC 6749 §6
//!   refresh-token flow.
//! - [`credentials`] — on-disk credentials store (mode `0o600` on Unix).
//! - [`commands`] — `login` / `logout` / `whoami` entry points wired by
//!   the CLI.

pub mod commands;
pub mod credentials;
pub mod flow;
pub mod metadata;
pub mod settings;
