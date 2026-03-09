#[cfg(any(feature = "openai", feature = "anthropic", feature = "google"))]
use std::fmt;

#[cfg(any(feature = "openai", feature = "anthropic", feature = "google"))]
use bitrouter_core::errors::BitrouterError;
#[cfg(any(feature = "openai", feature = "anthropic", feature = "google"))]
use warp::reject::Reject;

/// Wraps a [`BitrouterError`] so it can be used as a warp rejection.
#[derive(Debug)]
#[cfg(any(feature = "openai", feature = "anthropic", feature = "google"))]
pub(crate) struct BitrouterRejection(pub BitrouterError);

#[cfg(any(feature = "openai", feature = "anthropic", feature = "google"))]
impl fmt::Display for BitrouterRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(any(feature = "openai", feature = "anthropic", feature = "google"))]
impl Reject for BitrouterRejection {}

/// Wraps a generic message as a warp rejection.
#[derive(Debug)]
#[cfg(feature = "openai")]
pub(crate) struct BadRequest(pub String);

#[cfg(feature = "openai")]
impl fmt::Display for BadRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(feature = "openai")]
impl Reject for BadRequest {}
