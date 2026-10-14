//! Client transport failures and public service failures remain distinct.

use bitrouter_ai::gateway::GatewayErrorEnvelope;
use reqwest::header::HeaderMap;

/// A service failure with its HTTP metadata and optional structured body.
///
/// Callers may inspect authentication challenges and retry hints, but the SDK
/// never retries a request automatically. Debug output excludes headers/body.
pub struct ServiceFailure {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Option<GatewayErrorEnvelope>,
    /// Reading or decoding the optional error body failed; HTTP metadata remains authoritative.
    pub body_error: Option<Error>,
}

impl std::fmt::Debug for ServiceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceFailure")
            .field("status", &self.status)
            .field("body_error", &self.body_error)
            .finish_non_exhaustive()
    }
}

/// An SDK failure. Transport failure after dispatch can leave execution unknown.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid client options: {0}")]
    Options(&'static str),
    #[error("invalid request: {0}")]
    Request(&'static str),
    #[error("gateway transport failed; execution may have started")]
    Transport(#[source] reqwest::Error),
    #[error("gateway returned HTTP status {}", .0.status)]
    Service(Box<ServiceFailure>),
    #[error("gateway reported a stream failure")]
    Stream(Box<ServiceFailure>),
    #[error("invalid gateway response: {0}")]
    Response(&'static str),
    #[error("gateway response exceeded the configured byte limit")]
    Limit,
    #[error("gateway stream ended without its terminal event; execution may still be running")]
    IncompleteStream,
}

impl Error {
    pub(crate) fn transport(error: reqwest::Error) -> Self {
        Self::Transport(error.without_url())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
