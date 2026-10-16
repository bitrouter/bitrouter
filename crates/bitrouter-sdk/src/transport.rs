use futures_util::StreamExt;
use reqwest::{Response, header};

use crate::error::{Error, Result, ServiceFailure};

pub(crate) async fn body(response: Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(Error::Limit);
    }
    let mut chunks = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(Error::transport)?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(Error::Limit);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(crate) async fn successful(response: Response, limit: usize) -> Result<Response> {
    successful_as(response, limit, Error::Service).await
}

pub(crate) async fn successful_as<Body: serde::de::DeserializeOwned>(
    response: Response,
    limit: usize,
    failure: fn(Box<ServiceFailure<Body>>) -> Error,
) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    Err(failure(service_failure(response, limit).await))
}

pub(crate) async fn service_failure<Body: serde::de::DeserializeOwned>(
    response: Response,
    limit: usize,
) -> Box<ServiceFailure<Body>> {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let (body, body_error) = match body(response, limit).await {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(body) => (Some(body), None),
            Err(_) => (
                None,
                Some(Error::Response("invalid service error envelope")),
            ),
        },
        Err(error) => (None, Some(error)),
    };
    Box::new(ServiceFailure {
        status,
        headers,
        body,
        body_error,
    })
}

pub(crate) fn has_content_type(response: &Response, expected: &str) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}

pub(crate) fn build_http(
    headers: header::HeaderMap,
    options: &crate::gateway::TransportOptions,
    http1_only: bool,
) -> Result<reqwest::Client> {
    // Disable both redirect forwarding and reqwest's protocol-level retry
    // policy: a disconnected mutation/inference call has an unknown outcome.
    // https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html#method.retry
    let mut builder = reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(options.connect_timeout)
        .user_agent(concat!("bitrouter-sdk/", env!("CARGO_PKG_VERSION")));
    if !options.system_proxy {
        builder = builder.no_proxy();
    }
    if let Some(timeout) = options.read_timeout {
        builder = builder.read_timeout(timeout);
    }
    if let Some(timeout) = options.request_timeout {
        builder = builder.timeout(timeout);
    }
    if http1_only {
        // HTTP/1.1 upgrade needs a dedicated pool and ALPN selection even when
        // another consumer enables reqwest's HTTP/2 feature.
        // https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html#method.http1_only
        builder = builder.http1_only();
    }
    builder.build().map_err(Error::transport)
}
