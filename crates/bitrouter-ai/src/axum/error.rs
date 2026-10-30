//! Explicit public error presentation; never converts SDK/provider diagnostics.

use ::axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// A caller-selected, publicly safe gateway failure.
///
/// Callers must redact sensitive details before construction. The adapter never
/// forwards internal headers or raw diagnostic bodies. Invalid success/redirect
/// statuses or oversized strings are replaced with a generic internal error.
#[derive(Clone, Debug)]
pub struct Error {
    status: StatusCode,
    kind: String,
    code: String,
    message: String,
}

impl Error {
    /// Select the HTTP error status and safe public strings.
    /// Messages are limited to 4096 UTF-8 bytes, kind/code to 128 bytes each.
    /// Invalid inputs become a generic internal error before any input is copied.
    pub fn new(status: StatusCode, kind: &str, code: &str, message: &str) -> Self {
        if (!status.is_client_error() && !status.is_server_error())
            || message.len() > 4096
            || kind.len() > 128
            || code.len() > 128
        {
            return Self::internal();
        }
        Self {
            status,
            kind: kind.into(),
            code: code.into(),
            message: message.into(),
        }
    }

    /// Generic failure for invalid or incomplete relay output.
    pub fn bad_gateway() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "invalid_response",
            "upstream response is invalid or incomplete",
        )
    }

    /// Selected error status, available before a response is committed.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub(super) fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            kind: "internal_error".into(),
            code: "internal_error".into(),
            message: "internal server error".into(),
        }
    }

    fn detail(&self) -> Detail<'_> {
        Detail {
            message: &self.message,
            kind: &self.kind,
            code: &self.code,
        }
    }

    pub(super) fn envelope(&self) -> Envelope<'_> {
        Envelope {
            error: self.detail(),
        }
    }

    pub(super) fn messages_event(&self) -> MessagesError<'_> {
        MessagesError {
            kind: "error",
            error: self.detail(),
        }
    }

    pub(super) fn responses_event(&self, sequence: u64) -> ResponsesError<'_> {
        ResponsesError {
            kind: "error",
            code: &self.code,
            message: &self.message,
            param: None,
            sequence_number: sequence,
        }
    }
}

#[derive(Serialize)]
struct Detail<'a> {
    message: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    code: &'a str,
}
#[derive(Serialize)]
pub(super) struct Envelope<'a> {
    error: Detail<'a>,
}
#[derive(Serialize)]
pub(super) struct MessagesError<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    error: Detail<'a>,
}
#[derive(Serialize)]
pub(super) struct ResponsesError<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    code: &'a str,
    message: &'a str,
    param: Option<()>,
    sequence_number: u64,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "public gateway error ({})", self.status)
    }
}
impl std::error::Error for Error {}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match super::response::Reply::json(&self.envelope(), 32 * 1024) {
            Ok(reply) => (self.status, reply).into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR,
                [(::axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"error":{"message":"internal server error","type":"internal_error","code":"internal_error"}}"#
            ).into_response(),
        }
    }
}
