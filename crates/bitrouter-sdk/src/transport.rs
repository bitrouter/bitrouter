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
    if response.status().is_success() {
        return Ok(response);
    }
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
    Err(Error::Service(Box::new(ServiceFailure {
        status,
        headers,
        body,
        body_error,
    })))
}

pub(crate) fn has_content_type(response: &Response, expected: &str) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}
