//! Dedicated, credential-free HTTP transport for one complete Registry v1 file.

use super::RegistryData;
use bitrouter_registry::document::MAX_DOCUMENT_BYTES;
use std::{net::IpAddr, time::Duration};

/// Explicit permission to download catalog data.
#[derive(Debug, Clone, Copy)]
pub enum NetworkPolicy {
    /// No request is issued.
    Offline,
    /// A bounded complete GET; production uses fifteen seconds.
    Allowed { request_timeout: Duration },
}
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("catalog refresh disabled by network policy")]
    Offline,
    #[error("invalid registry document URL; use HTTPS or an explicit literal loopback mirror")]
    InvalidSource,
    #[error("registry download failed; retaining the last admitted document")]
    Network,
    #[error("registry response must be HTTP 200 and fit the document resource budget")]
    Incomplete,
    #[error("{0}")]
    Parse(#[source] anyhow::Error),
}

/// Fetch with an isolated client. The caller's inference client is deliberately
/// not reused: redirects, ambient headers and credentials are not authorized.
pub async fn fetch_registry(
    _client: &reqwest::Client,
    source: &str,
    policy: NetworkPolicy,
) -> Result<RegistryData, FetchError> {
    let NetworkPolicy::Allowed { request_timeout } = policy else {
        return Err(FetchError::Offline);
    };
    let url = reqwest::Url::parse(source).map_err(|_| FetchError::InvalidSource)?;
    let loopback = url
        .host_str()
        .and_then(|s| s.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback());
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || request_timeout.is_zero()
    {
        return Err(FetchError::InvalidSource);
    }
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(request_timeout.min(Duration::from_secs(15)));
    if loopback {
        builder = builder.no_proxy();
    }
    let client = builder.build().map_err(|_| FetchError::Network)?;
    let mut response = client
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            concat!("bitrouter/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .map_err(|_| FetchError::Network)?;
    if response.status() != reqwest::StatusCode::OK
        || response
            .content_length()
            .is_some_and(|len| len > MAX_DOCUMENT_BYTES as u64)
    {
        return Err(FetchError::Incomplete);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| FetchError::Network)? {
        if chunk.len() > MAX_DOCUMENT_BYTES.saturating_sub(bytes.len()) {
            return Err(FetchError::Incomplete);
        }
        bytes.extend_from_slice(&chunk);
    }
    super::decode::from_slice(&bytes).map_err(FetchError::Parse)
}
