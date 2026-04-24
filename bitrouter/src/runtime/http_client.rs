//! Shared `reqwest` client construction for upstream HTTP calls.
//!
//! Centralises timeout configuration so model and tool routers fail fast on
//! stalled upstream providers instead of leaving requests hanging
//! indefinitely. The defaults are chosen to bound mid-stream stalls while
//! still allowing slow first-token generation.

use std::time::Duration;

/// Maximum time allowed to establish a TCP+TLS connection to upstream.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum time allowed between successive bytes on a response body.
///
/// Bounds idle stalls on streaming (SSE) responses without limiting the
/// total stream duration. Generous enough to accommodate long first-token
/// latencies on reasoning models, short enough to surface dead connections
/// in a reasonable time window.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// How long an idle pooled connection is kept before being closed.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// TCP keepalive interval used to detect half-open connections to upstream.
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);

/// Build the shared `reqwest::Client` used for upstream model and tool
/// calls.
///
/// Falls back to `reqwest::Client::new()` if the configured builder fails
/// (which only happens when the TLS backend cannot be initialised); the
/// fallback preserves prior behaviour rather than aborting the server.
pub fn build_upstream_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .build()
        .unwrap_or_else(|error| {
            tracing::warn!(
                "failed to build upstream HTTP client with timeouts ({error}); \
                 falling back to default client"
            );
            reqwest::Client::new()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_upstream_client_succeeds() {
        let _client = build_upstream_client();
    }
}
