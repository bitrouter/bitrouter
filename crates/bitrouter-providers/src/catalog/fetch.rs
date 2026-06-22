//! Fetch the public model catalog from <https://models.dev/api.json>.
//!
//! Used to enrich a `models_dev` auto-sync provider's catalog with its FULL
//! model set (beyond the registry-curated canonical subset). Best-effort: a
//! failure leaves the curated models in place (see the catalog apply step).

use crate::catalog::types::Catalog;

/// The public catalog endpoint. Documented at <https://models.dev/api>.
pub const CATALOG_URL: &str = "https://models.dev/api.json";

/// User-agent string sent with catalog fetches — helps the upstream isolate
/// bitrouter traffic in their logs.
const USER_AGENT: &str = concat!("bitrouter/", env!("CARGO_PKG_VERSION"));

/// Errors that can arise while pulling the catalog over the network.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// Transport-level failure (DNS, TCP, TLS, HTTP status, …).
    #[error("network error fetching the model catalog from models.dev: {0}")]
    Network(#[source] reqwest::Error),
    /// HTTP succeeded but the body wasn't valid catalog JSON.
    #[error("malformed catalog JSON from models.dev: {0}")]
    Parse(#[source] serde_json::Error),
}

/// Download + parse the catalog from [`CATALOG_URL`].
///
/// Bounded `connect_timeout` + overall `timeout` (`rustls-tls`, the workspace's
/// feature pin) so an unreachable models.dev fails fast on a no-network host
/// rather than stalling startup — mirrors the registry fetch policy.
pub async fn fetch_catalog() -> Result<Catalog, FetchError> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(FetchError::Network)?;
    let body = client
        .get(CATALOG_URL)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(FetchError::Network)?
        .text()
        .await
        .map_err(FetchError::Network)?;
    serde_json::from_str(&body).map_err(FetchError::Parse)
}
