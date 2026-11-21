//! Typed JSON extraction using Axum's configured body limit.

use ::axum::{
    extract::{FromRequest, Request},
    http::StatusCode,
};
use serde::de::DeserializeOwned;

use super::error::Error;

/// JSON extractor with structured, redacted gateway rejections.
///
/// Uses Axum's `DefaultBodyLimit` (2 MiB unless the host changes it). Put this
/// extractor last after authentication/parts extractors. Hosts must configure a
/// finite limit appropriate for their model input; this adapter never disables it.
/// <https://docs.rs/axum/0.8.9/axum/extract/struct.DefaultBodyLimit.html>
pub struct Json<T>(pub T);

impl<T, S> FromRequest<S> for Json<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Error;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        ::axum::Json::<T>::from_request(request, state)
            .await
            .map(|value| Self(value.0))
            .map_err(|rejection| {
                let status = rejection.status();
                let message = match status {
                    StatusCode::PAYLOAD_TOO_LARGE => "request body exceeds its byte limit",
                    StatusCode::UNSUPPORTED_MEDIA_TYPE => "expected application/json",
                    _ => "request body does not match the public JSON contract",
                };
                Error::new(status, "invalid_request_error", "invalid_request", message)
            })
    }
}
