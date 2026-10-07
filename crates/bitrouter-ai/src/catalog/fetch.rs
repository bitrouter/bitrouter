//! Fetch a complete catalog only under the caller's explicit network policy.

use std::time::Duration;

use super::types::{CanonicalModel, Envelope, RegistryData, providers_from_values};

/// Network permission for a requested catalog refresh.
#[derive(Debug, Clone, Copy)]
pub enum NetworkPolicy {
    /// Reject discovery without issuing HTTP requests.
    Offline,
    /// Allow bounded requests using the caller's client and source URL.
    Allowed {
        /// Overall timeout for each artifact, including reading its body.
        request_timeout: Duration,
    },
}

/// Failures while fetching a catalog. Partial results are never published.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The caller forbids network discovery.
    #[error("catalog refresh is disabled by network policy")]
    Offline,
    /// The base URL is not a plain HTTP(S) catalog location.
    #[error("invalid catalog source URL")]
    InvalidSource,
    /// A transport/status failure, without response body contents.
    #[error("catalog request failed: {0}")]
    Network(#[source] reqwest::Error),
    /// An artifact is not valid catalog JSON.
    #[error("invalid catalog JSON: {0}")]
    Parse(#[source] serde_json::Error),
}

/// Fetch both artifacts using only the supplied client, URL and network policy.
pub async fn fetch_registry(
    client: &reqwest::Client,
    base: &str,
    policy: NetworkPolicy,
) -> Result<RegistryData, FetchError> {
    let NetworkPolicy::Allowed { request_timeout } = policy else {
        return Err(FetchError::Offline);
    };
    let url = reqwest::Url::parse(base).map_err(|_| FetchError::InvalidSource)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || request_timeout.is_zero()
    {
        return Err(FetchError::InvalidSource);
    }
    let base = base.trim_end_matches('/');
    let providers: Envelope<serde_json::Value> =
        fetch_envelope(client, &format!("{base}/providers.json"), request_timeout).await?;
    let models: Envelope<CanonicalModel> =
        fetch_envelope(client, &format!("{base}/models.json"), request_timeout).await?;
    Ok(RegistryData {
        providers: providers_from_values(providers.data).map_err(FetchError::Parse)?,
        canonical: models.data,
    })
}

async fn fetch_envelope<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<Envelope<T>, FetchError> {
    let body = client
        .get(url)
        .timeout(timeout)
        .header(
            reqwest::header::USER_AGENT,
            concat!("bitrouter/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(FetchError::Network)?
        .bytes()
        .await
        .map_err(FetchError::Network)?;
    serde_json::from_slice(&body).map_err(FetchError::Parse)
}
