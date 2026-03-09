//! Warp filter for extracting and validating an [`AuthContext`] from the request.

use std::sync::Arc;

use bitrouter_core::server::{
    auth::{AuthContext, AuthSubject, Authenticator},
    errors::ServerError,
};
use warp::Filter;

use crate::error::ServerRejection;

/// Creates a warp filter that extracts and validates an [`AuthContext`] from the request.
///
/// Reads the `Authorization` header, parses it as a Bearer token or API key,
/// and authenticates via the provided [`Authenticator`] implementation.
pub fn with_auth<A>(
    authenticator: Arc<A>,
) -> impl Filter<Extract = (AuthContext,), Error = warp::Rejection> + Clone
where
    A: Authenticator + Send + Sync + 'static,
{
    warp::header::<String>("authorization")
        .or(warp::any().map(String::new))
        .unify()
        .and(warp::any().map(move || authenticator.clone()))
        .and_then(extract_auth)
}

async fn extract_auth<A>(
    authorization: String,
    authenticator: Arc<A>,
) -> Result<AuthContext, warp::Rejection>
where
    A: Authenticator + Send + Sync + 'static,
{
    let subject =
        parse_auth_header(&authorization).map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    authenticator
        .authenticate(&subject)
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))
}

fn parse_auth_header(header: &str) -> Result<AuthSubject, ServerError> {
    let header = header.trim();
    if header.is_empty() {
        return Err(ServerError::unauthorized("missing authorization header"));
    }
    if let Some(token) = header.strip_prefix("Bearer ") {
        Ok(AuthSubject::Bearer(token.trim().to_owned()))
    } else {
        Ok(AuthSubject::ApiKey(header.to_owned()))
    }
}
