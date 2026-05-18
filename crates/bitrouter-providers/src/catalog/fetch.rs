//! Fetch the live model catalog from <https://models.dev/api.json>.

use crate::catalog::types::Catalog;

/// The canonical catalog endpoint. Documented at <https://models.dev/api>.
pub const CATALOG_URL: &str = "https://models.dev/api.json";

/// User-agent string sent with catalog fetches — helps the upstream
/// understand traffic patterns and isolate bitrouter calls in their logs.
const USER_AGENT: &str = concat!("bitrouter/", env!("CARGO_PKG_VERSION"));

/// Errors that can arise while pulling the catalog over the network.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// Transport-level failure (DNS, TCP, TLS, HTTP status, …).
    #[error("network error fetching {CATALOG_URL}: {0}")]
    Network(#[from] reqwest::Error),
    /// HTTP succeeded but the body wasn't a valid catalog JSON.
    #[error("malformed catalog JSON at {CATALOG_URL}: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Download + parse the catalog from [`CATALOG_URL`].
///
/// Plain `reqwest::get` with `rustls-tls` (the workspace's feature pin) and a
/// per-call 30s timeout. Callers that want a longer-lived client should call
/// [`fetch_catalog_with`] with their own [`reqwest::Client`].
pub async fn fetch_catalog() -> Result<Catalog, FetchError> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    fetch_catalog_with(&client).await
}

/// Fetch the catalog using a caller-owned [`reqwest::Client`]. Useful when
/// the caller already runs a connection pool / proxy / mTLS setup.
pub async fn fetch_catalog_with(client: &reqwest::Client) -> Result<Catalog, FetchError> {
    let body = client
        .get(CATALOG_URL)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let catalog = serde_json::from_str(&body)?;
    Ok(catalog)
}
