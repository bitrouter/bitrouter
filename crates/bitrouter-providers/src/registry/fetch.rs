//! Fetch the provider-registry distribution artifacts over the network.
//!
//! Source: the public registry repo's generated `dist/` directory, served raw
//! from GitHub. See <https://github.com/bitrouter/provider-registry>. Two files
//! are read and merged into one [`RegistryData`]:
//! `{base}/providers.json` and `{base}/canonical.json`.

use crate::registry::types::{CanonicalModel, Envelope, RegistryData, RegistryProvider};

/// User-agent string sent with registry fetches — helps the upstream isolate
/// bitrouter traffic in their logs.
const USER_AGENT: &str = concat!("bitrouter/", env!("CARGO_PKG_VERSION"));

/// Errors that can arise while pulling the registry over the network.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// Transport-level failure (DNS, TCP, TLS, HTTP status, …).
    #[error("network error fetching registry from {base}: {source}")]
    Network {
        /// The base URL the fetch was made against.
        base: String,
        /// The underlying transport error.
        #[source]
        source: reqwest::Error,
    },
    /// HTTP succeeded but a body wasn't valid registry JSON.
    #[error("malformed registry JSON from {base}: {source}")]
    Parse {
        /// The base URL the fetch was made against.
        base: String,
        /// The underlying JSON parse error.
        #[source]
        source: serde_json::Error,
    },
}

/// Download + parse the registry from `base` (the base URL of the `dist/`
/// directory; defaults to [`bitrouter_sdk::config::DEFAULT_REGISTRY_URL`] via
/// [`RegistryConfig`](bitrouter_sdk::config::RegistryConfig)).
///
/// Bounded `connect_timeout` + overall `timeout` (`rustls-tls`, the workspace's
/// feature pin) so an unreachable registry fails fast on a no-network host
/// rather than stalling startup for the OS-level connect window. Callers that
/// want a longer-lived client should call [`fetch_registry_with`] with their
/// own [`reqwest::Client`].
pub async fn fetch_registry(base: &str) -> Result<RegistryData, FetchError> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|source| FetchError::Network {
            base: base.to_string(),
            source,
        })?;
    fetch_registry_with(&client, base).await
}

/// Fetch the registry using a caller-owned [`reqwest::Client`]. Reads
/// `providers.json` then `canonical.json` under `base` and merges them.
pub async fn fetch_registry_with(
    client: &reqwest::Client,
    base: &str,
) -> Result<RegistryData, FetchError> {
    let base_trimmed = base.trim_end_matches('/');
    let providers: Envelope<RegistryProvider> =
        fetch_envelope(client, base, &format!("{base_trimmed}/providers.json")).await?;
    let canonical: Envelope<CanonicalModel> =
        fetch_envelope(client, base, &format!("{base_trimmed}/canonical.json")).await?;
    Ok(RegistryData {
        providers: providers.data,
        canonical: canonical.data,
    })
}

/// GET one `{ "data": [ … ] }` artifact and parse it.
async fn fetch_envelope<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    base: &str,
    url: &str,
) -> Result<Envelope<T>, FetchError> {
    let body = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|source| FetchError::Network {
            base: base.to_string(),
            source,
        })?
        .text()
        .await
        .map_err(|source| FetchError::Network {
            base: base.to_string(),
            source,
        })?;
    serde_json::from_str(&body).map_err(|source| FetchError::Parse {
        base: base.to_string(),
        source,
    })
}
