//! OTLP/HTTP client that injects a freshly-resolved bearer per export.
//!
//! Wraps a [`reqwest::Client`] and implements [`opentelemetry_http::HttpClient`]
//! so the OTLP batch processor calls *us* on every export. Before delegating to
//! the inner reqwest client we resolve a live bearer via [`TelemetryBearer`] and
//! stamp `Authorization: Bearer <token>` onto the outgoing request — picking up a
//! refreshed account token without a daemon restart, which a static header
//! ([`crate::otel::config::OtelConfig::bearer_token`]) cannot do.
//!
//! `opentelemetry-http` 0.27 trait:
//! <https://docs.rs/opentelemetry-http/0.27/opentelemetry_http/trait.HttpClient.html>
//!
//! Best-effort: a bearer-resolution failure (mapped to `None`) leaves the
//! request unauthenticated rather than dropping the export — telemetry must
//! never break for want of a token. An `Authorization` header already on the
//! request (e.g. an operator-supplied one) is never clobbered.

use std::sync::Arc;

use bytes::Bytes;
use opentelemetry_http::{HttpClient, HttpError};

use crate::otel::bearer::TelemetryBearer;

/// `opentelemetry_http::HttpClient` wrapper that injects a live bearer per export.
#[derive(Debug)]
pub(crate) struct AuthRefreshClient {
    inner: reqwest::Client,
    bearer: Arc<dyn TelemetryBearer>,
}

impl AuthRefreshClient {
    /// Build a client that resolves `bearer` on every export and delegates the
    /// actual transport to `inner`.
    pub(crate) fn new(inner: reqwest::Client, bearer: Arc<dyn TelemetryBearer>) -> Self {
        Self { inner, bearer }
    }
}

/// Stamp `Authorization: Bearer <token>` onto `req` when `token` is `Some` and
/// the request does not already carry an `authorization` header (matched
/// case-insensitively — HTTP header names are case-insensitive). A `None` token,
/// or an unrepresentable header value, leaves the request unchanged (anonymous).
///
/// Pure (no I/O) so the header policy is unit-testable without a live server.
fn inject_bearer(req: &mut http::Request<Vec<u8>>, token: Option<&str>) {
    let Some(token) = token else {
        return;
    };
    if req.headers().contains_key(http::header::AUTHORIZATION) {
        // `HeaderMap::contains_key` matches the canonical `authorization` name
        // case-insensitively, so an operator-supplied auth header (in any case)
        // is preserved rather than clobbered.
        return;
    }
    if let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
        req.headers_mut().insert(http::header::AUTHORIZATION, value);
    }
}

#[async_trait::async_trait]
impl HttpClient for AuthRefreshClient {
    async fn send(
        &self,
        mut request: http::Request<Vec<u8>>,
    ) -> Result<http::Response<Bytes>, HttpError> {
        // Best-effort: a resolution failure inside `bearer()` is mapped to
        // `None` by the implementor, so a `None` here simply means "export
        // anonymously" — never a dropped export.
        let token = self.bearer.bearer().await;
        inject_bearer(&mut request, token.as_deref());
        <reqwest::Client as HttpClient>::send(&self.inner, request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_request() -> http::Request<Vec<u8>> {
        http::Request::builder()
            .method(http::Method::POST)
            .uri("https://telemetry.example/v1/traces")
            .body(Vec::new())
            .unwrap()
    }

    fn auth_header(req: &http::Request<Vec<u8>>) -> Option<&str> {
        req.headers()
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
    }

    #[test]
    fn injects_bearer_when_some_and_absent() {
        let mut req = empty_request();
        inject_bearer(&mut req, Some("tok"));
        assert_eq!(auth_header(&req), Some("Bearer tok"));
    }

    #[test]
    fn leaves_request_unchanged_when_none() {
        let mut req = empty_request();
        inject_bearer(&mut req, None);
        assert!(
            auth_header(&req).is_none(),
            "a None bearer must leave the request anonymous"
        );
    }

    #[test]
    fn does_not_clobber_existing_authorization_header() {
        let mut req = empty_request();
        req.headers_mut().insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer existing"),
        );
        inject_bearer(&mut req, Some("tok"));
        assert_eq!(
            auth_header(&req),
            Some("Bearer existing"),
            "an existing Authorization header must not be overwritten"
        );
    }

    /// Stub `TelemetryBearer` returning a fixed token (or `None`) so the
    /// `HttpClient::send` path can be exercised without a live server.
    #[derive(Debug)]
    struct StubBearer(Option<String>);

    #[async_trait::async_trait]
    impl TelemetryBearer for StubBearer {
        async fn bearer(&self) -> Option<String> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn stub_bearer_drives_injection_helper() {
        // Exercise the same resolve-then-inject sequence `send` uses, proving
        // the trait + helper compose: Some(token) injects, None stays anonymous.
        let some = StubBearer(Some("tok".into()));
        let mut req = empty_request();
        inject_bearer(&mut req, some.bearer().await.as_deref());
        assert_eq!(auth_header(&req), Some("Bearer tok"));

        let none = StubBearer(None);
        let mut req = empty_request();
        inject_bearer(&mut req, none.bearer().await.as_deref());
        assert!(auth_header(&req).is_none());
    }
}
