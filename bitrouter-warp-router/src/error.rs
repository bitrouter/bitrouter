use std::fmt;

use bitrouter_core::errors::BitrouterError;
use warp::reject::Reject;

/// Wraps a [`BitrouterError`] so it can be used as a warp rejection.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct BitrouterRejection(pub BitrouterError);

impl fmt::Display for BitrouterRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Reject for BitrouterRejection {}

/// Wraps a generic message as a warp rejection.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct BadRequest(pub String);

impl fmt::Display for BadRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Reject for BadRequest {}
