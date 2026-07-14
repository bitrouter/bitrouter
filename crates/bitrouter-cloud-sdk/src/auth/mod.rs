//! Sign-in to BitRouter Cloud via an API key or the OAuth 2.0 Device
//! Authorization Grant (RFC 8628). This module is distinct from the
//! upstream-provider OAuth code in `bitrouter-providers`:
//!
//! - `bitrouter-providers` ships device-code + auth-code clients used to
//!   sign the user *into a third-party LLM vendor* (Anthropic Pro/Max,
//!   ChatGPT, GitHub Copilot). Those tokens authenticate outbound calls
//!   to the vendor's API.
//! - This module signs the CLI *into a BitRouter user account* on the
//!   configured authorization server. The resulting bearer authenticates
//!   inbound calls to the BitRouter Cloud `/v1/*` surface (inference,
//!   key management, BYOK, policy, billing, …).
//!
//! Authoritative references:
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
