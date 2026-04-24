//! Helpers for parsing request bodies with verbose, debuggable error messages.
//!
//! Warp's built-in [`warp::body::json`] surfaces failures as an opaque
//! `Request body deserialize error: <serde error>` without any indication of
//! what payload the server actually received. This module provides a drop-in
//! replacement that:
//!
//! * deserializes the request body as JSON into the target type,
//! * on failure, reports the target type, the line/column where parsing
//!   failed, the underlying serde error, and a preview of the actual body so
//!   clients can debug bad requests without needing extra server-side logging.

use serde::de::DeserializeOwned;
use warp::Filter;

use crate::error::BadRequest;

/// Maximum number of bytes from the request body to include in error messages.
///
/// Large enough to capture realistic request payloads while bounding the size
/// of the surfaced error response.
const PREVIEW_LIMIT: usize = 4096;

/// Returns a warp filter that deserializes the JSON request body into `T`,
/// emitting a verbose [`BadRequest`] rejection on failure.
pub(crate) fn json<T>() -> impl Filter<Extract = (T,), Error = warp::Rejection> + Clone
where
    T: DeserializeOwned + Send + 'static,
{
    warp::body::bytes().and_then(|body: bytes::Bytes| async move {
        match serde_json::from_slice::<T>(body.as_ref()) {
            Ok(value) => Ok(value),
            Err(err) => Err(warp::reject::custom(BadRequest(format_error::<T>(
                body.as_ref(),
                &err,
            )))),
        }
    })
}

fn format_error<T>(body: &[u8], err: &serde_json::Error) -> String {
    let target = short_type_name(std::any::type_name::<T>());
    let preview = preview_body(body);
    format!(
        "failed to deserialize request body into `{target}`: {err} \
         (at line {line} column {column}); request body was: {preview}",
        line = err.line(),
        column = err.column(),
    )
}

/// Returns the final segment of a fully-qualified Rust type name, e.g.
/// `bitrouter_core::api::openai::chat::ChatCompletionRequest` becomes
/// `ChatCompletionRequest`.
fn short_type_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

fn preview_body(body: &[u8]) -> String {
    if body.is_empty() {
        return "<empty>".to_owned();
    }
    let truncated = body.len() > PREVIEW_LIMIT;
    let slice = if truncated {
        &body[..PREVIEW_LIMIT]
    } else {
        body
    };
    let mut text = String::from_utf8_lossy(slice).into_owned();
    if truncated {
        use std::fmt::Write as _;
        let _ = write!(text, "... [truncated, {} bytes total]", body.len());
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_body_handles_empty_input() {
        assert_eq!(preview_body(b""), "<empty>");
    }

    #[test]
    fn preview_body_returns_full_payload_when_short() {
        assert_eq!(preview_body(b"{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn preview_body_truncates_long_payloads() {
        let body = vec![b'a'; PREVIEW_LIMIT + 100];
        let preview = preview_body(&body);
        assert!(preview.starts_with(&"a".repeat(PREVIEW_LIMIT)));
        assert!(preview.ends_with(&format!(
            "... [truncated, {} bytes total]",
            PREVIEW_LIMIT + 100
        )));
    }

    #[test]
    fn short_type_name_strips_module_path() {
        assert_eq!(
            short_type_name("bitrouter_core::api::openai::chat::ChatCompletionRequest"),
            "ChatCompletionRequest"
        );
        assert_eq!(short_type_name("Plain"), "Plain");
    }

    #[test]
    fn format_error_includes_target_type_position_and_body() {
        #[derive(Debug, serde::Deserialize)]
        struct Sample {
            value: u32,
        }

        let body = br#"{"value": "not a number"}"#;
        let err = serde_json::from_slice::<Sample>(body).unwrap_err();
        let message = format_error::<Sample>(body, &err);

        assert!(message.contains("Sample"), "missing target type: {message}");
        assert!(message.contains("line"), "missing position info: {message}");
        assert!(
            message.contains(r#"{"value": "not a number"}"#),
            "missing request body preview: {message}",
        );

        // Also exercise a successful parse so the `value` field is read,
        // confirming the type is non-trivial.
        let ok_body = br#"{"value": 7}"#;
        let parsed: Sample = serde_json::from_slice(ok_body).unwrap();
        assert_eq!(parsed.value, 7);
    }

    #[tokio::test]
    async fn json_filter_returns_verbose_400_with_body_preview() {
        use crate::error::handle_bitrouter_rejection;
        use warp::Filter;

        #[derive(Debug, serde::Deserialize, serde::Serialize)]
        struct Sample {
            value: u32,
        }

        let filter = warp::post()
            .and(super::json::<Sample>())
            .map(|_: Sample| warp::reply::reply())
            .recover(|err: warp::Rejection| async move {
                if let Some(resp) = handle_bitrouter_rejection(&err) {
                    Ok(resp)
                } else {
                    Err(err)
                }
            });

        let raw_body = r#"{"value": "oops"}"#;
        let res = warp::test::request()
            .method("POST")
            .header("content-type", "application/json")
            .body(raw_body)
            .reply(&filter)
            .await;

        assert_eq!(res.status(), 400);
        let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("Sample"),
            "expected target type in message: {message}"
        );
        assert!(
            message.contains(raw_body),
            "expected request body preview in message: {message}"
        );
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }
}
