//! Wire-form error envelope returned by the BitRouter Cloud `/v1/*`
//! management surface, mirrored here so the CLI can match on the same
//! taxonomy without depending on the server crate.
//!
//! The server response shape is `{ "error": <code>, "error_description":
//! <msg> }` per `bitrouter_cloud::v1::http::management::error`. Codes
//! are: `bad_request | unauthorized | forbidden | not_found | conflict |
//! internal`.

use serde::Deserialize;
use thiserror::Error;

/// Error returned by every [`crate::management::ManagementClient`] call.
///
/// Variants 1:1 with the server's wire error codes, plus the local
/// failure modes (no credentials, network, JSON decode). The CLI maps
/// each onto a user-facing message; [`Error::missing_scope`] gives the
/// scope-suggestion path a structured hook to react to a 403.
#[derive(Debug, Error)]
pub enum Error {
    /// No credentials on disk — the user has not yet run
    /// `bitrouter auth login`.
    #[error("not signed in — run `bitrouter auth login` first")]
    NotSignedIn,

    /// OAuth token resolution or refresh failed (e.g. refresh token
    /// expired, AS metadata unreachable). The user should re-run
    /// `bitrouter auth login`.
    #[error("failed to resolve a BitRouter Cloud access token: {0:#}")]
    Auth(#[source] anyhow::Error),

    /// Server-reported `400 bad_request`. The body's `error_description`
    /// is carried in `message`.
    #[error("bad request: {message}")]
    BadRequest {
        /// Human-facing description from the server.
        message: String,
    },

    /// Server-reported `401 unauthorized` — the bearer was rejected
    /// after we attached it. Usually means the token has been revoked
    /// or the user logged out from another machine.
    #[error("unauthorized: {message}")]
    Unauthorized {
        /// Human-facing description from the server.
        message: String,
    },

    /// Server-reported `403 forbidden` — credential verified but the
    /// principal lacks the required scope. When `missing_scope` is
    /// populated the CLI prints a hint suggesting re-login with the
    /// missing scope appended.
    #[error("forbidden: {message}")]
    Forbidden {
        /// Human-facing description from the server.
        message: String,
        /// Parsed scope name when the description matches the
        /// server's `missing required scope: <scope>` format.
        missing_scope: Option<String>,
    },

    /// Server-reported `404 not_found`.
    #[error("not found: {message}")]
    NotFound {
        /// Human-facing description from the server.
        message: String,
    },

    /// Server-reported `409 conflict`.
    #[error("conflict: {message}")]
    Conflict {
        /// Human-facing description from the server.
        message: String,
    },

    /// Server-reported `500 internal` or any other unexpected HTTP
    /// status (e.g. 502 from a CDN, 503 during a deploy). `status` is
    /// the raw HTTP code.
    #[error("server error (HTTP {status}): {message}")]
    Server {
        /// Raw HTTP status code.
        status: u16,
        /// Best-effort message — either the server's
        /// `error_description` or the literal response body.
        message: String,
    },

    /// Reqwest-level transport failure (DNS, TLS, timeout, connection
    /// refused). The inner error preserves the original cause.
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// Response body was not valid JSON or did not match the expected
    /// schema.
    #[error("decoding server response: {0}")]
    Decode(#[from] serde_json::Error),
}

impl Error {
    /// When this is a 403 carrying a `missing required scope: <name>`
    /// description, return the scope name. Used by the CLI to print
    /// the `bitrouter auth login --scope …` hint.
    pub fn missing_scope(&self) -> Option<&str> {
        match self {
            Error::Forbidden { missing_scope, .. } => missing_scope.as_deref(),
            _ => None,
        }
    }
}

/// Server-side error envelope as defined by
/// `bitrouter_cloud::v1::http::management::error::ErrorBody`.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct ErrorBody {
    pub error: String,
    pub error_description: String,
}

impl ErrorBody {
    /// Promote a deserialised body + status into an [`Error`].
    pub(super) fn into_error(self, status: u16) -> Error {
        match self.error.as_str() {
            "bad_request" => Error::BadRequest {
                message: self.error_description,
            },
            "unauthorized" => Error::Unauthorized {
                message: self.error_description,
            },
            "forbidden" => {
                let missing_scope = parse_missing_scope(&self.error_description);
                Error::Forbidden {
                    message: self.error_description,
                    missing_scope,
                }
            }
            "not_found" => Error::NotFound {
                message: self.error_description,
            },
            "conflict" => Error::Conflict {
                message: self.error_description,
            },
            _ => Error::Server {
                status,
                message: self.error_description,
            },
        }
    }
}

/// Server-side `require_scope` rejection text is exactly
/// `"missing required scope: <scope>"` (see
/// `bitrouter_cloud::v1::http::management::auth::require_scope`).
/// Match that prefix exactly so we never mis-parse a future, broader
/// 403 message into a fake scope hint.
fn parse_missing_scope(description: &str) -> Option<String> {
    let trimmed = description.trim();
    trimmed
        .strip_prefix("missing required scope: ")
        .map(|s| s.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_maps_to_forbidden_with_scope() {
        let body = ErrorBody {
            error: "forbidden".into(),
            error_description: "missing required scope: keys:write".into(),
        };
        let err = body.into_error(403);
        assert_eq!(err.missing_scope(), Some("keys:write"));
        assert!(matches!(err, Error::Forbidden { .. }));
    }

    #[test]
    fn body_maps_unknown_code_to_server() {
        let body = ErrorBody {
            error: "something_new".into(),
            error_description: "huh".into(),
        };
        let err = body.into_error(500);
        match err {
            Error::Server { status, message } => {
                assert_eq!(status, 500);
                assert_eq!(message, "huh");
            }
            other => panic!("expected Server, got {other:?}"),
        }
    }

    #[test]
    fn missing_scope_is_none_for_other_403_descriptions() {
        let body = ErrorBody {
            error: "forbidden".into(),
            error_description: "calling principal has no account_id".into(),
        };
        let err = body.into_error(403);
        assert_eq!(err.missing_scope(), None);
    }
}
