//! Reusable Warp filters for BitRouter's HTTP surface.
//!
//! # Configuring the upstream HTTP client
//!
//! The filters in this crate are transport-agnostic: they delegate to a
//! [`bitrouter_core::routers::router::LanguageModelRouter`] implementation
//! supplied by the embedder. That router is responsible for owning the
//! `reqwest::Client` (or `reqwest_middleware::ClientWithMiddleware`) used
//! for upstream provider calls.
//!
//! **SDK users should configure that client with sensible timeouts.** A
//! bare `reqwest::Client::new()` has *no* timeouts, so a stalled upstream
//! SSE stream will leave the inbound request hanging indefinitely instead
//! of surfacing an `upstream_error` to the caller. At minimum, set:
//!
//! - `connect_timeout` — bound TCP+TLS handshake (e.g. `30s`).
//! - `read_timeout` — bound the gap *between* response-body bytes (e.g.
//!   `120s`). This catches mid-stream stalls without capping the overall
//!   stream duration, so legitimate long-running streams still complete.
//! - `pool_idle_timeout` — recycle dead pooled connections.
//! - `tcp_keepalive` — detect half-open connections.
//!
//! The reference binary's `bitrouter::runtime::http_client::build_upstream_client`
//! shows a working configuration that embedders can copy.
//!
//! Avoid using `reqwest::ClientBuilder::timeout` for routers that proxy
//! streaming endpoints (Anthropic Messages, OpenAI streaming chat, etc.):
//! it caps the *entire* request duration including the streamed body and
//! will truncate long responses.

pub mod router;

#[cfg(any(feature = "payments-tempo", feature = "payments-solana"))]
pub mod mpp;

#[cfg(feature = "accounts")]
pub mod accounts;
#[cfg(feature = "guardrails")]
pub mod guardrails;
#[cfg(feature = "observe")]
pub mod observe;

mod body;
pub mod error;
mod util;
