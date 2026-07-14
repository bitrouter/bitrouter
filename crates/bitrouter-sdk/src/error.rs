//! Crate-wide error type.
//!
//! Every fallible SDK call returns [`Result<T>`](Result) (an alias for
//! `std::result::Result<T, BitrouterError>`). Variants carry an HTTP status so
//! the server handlers can render OpenAI/Anthropic-style error envelopes
//! without a separate mapping table. The type is `Clone` because the pipeline
//! hands the same error to several consumers (observe hooks, settlement
//! recorders, the caller).
//!
//! [`BitrouterError::to_envelope`] projects an error into the canonical,
//! serializable [`ErrorEnvelope`] (`{"error": {"kind": …, "message": …}}`) —
//! the stable typed shape the CLI prints on stdout. HTTP surfaces use their
//! protocol-specific projections; provider bad requests preserve the selected
//! upstream `error` object or string.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, BitrouterError>;

/// The unified BitRouter error type.
///
/// Variants carry an HTTP status so handlers can render OpenAI/Anthropic-style
/// error envelopes without a separate mapping table. `Clone` because the
/// pipeline hands the same error to several consumers (observe hooks,
/// settlement recorders, the caller).
#[derive(Debug, Clone, thiserror::Error)]
pub enum BitrouterError {
    /// 400 — malformed request (bad JSON, unknown enum variant, …).
    #[error("bad request: {message}")]
    BadRequest {
        /// Human-readable detail.
        message: String,
    },

    /// 401 — missing or invalid credentials.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// 402 — payment required (MPP challenge / insufficient balance).
    #[error("payment required: {0}")]
    PaymentRequired(String),

    /// 402 — an upstream provider account has insufficient credit.
    #[error("upstream payment required")]
    UpstreamPaymentRequired,

    /// 403 — policy violation.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// 404 — no route found for the requested model.
    #[error("not found: {0}")]
    NotFound(String),

    /// 429 — rate limited.
    #[error("rate limited")]
    RateLimited {
        /// Seconds the caller should wait before retrying.
        retry_after: Option<u64>,
    },

    /// 429 — every usable upstream route is temporarily rate limited.
    #[error("upstream rate limited")]
    UpstreamRateLimited {
        /// Seconds the caller should wait before retrying the earliest route.
        retry_after: Option<u64>,
    },

    /// 400 — an upstream provider rejected the request parameters.
    #[error("upstream bad request")]
    UpstreamBadRequest {
        /// Upstream-selected error payload. Executor-produced values are
        /// always a JSON object or JSON string.
        error: serde_json::Value,
    },

    /// 502 — upstream provider returned an error.
    #[error("upstream error ({status}): {message}")]
    Upstream {
        /// Upstream HTTP status.
        status: u16,
        /// Upstream error body / detail.
        message: String,
    },

    /// 502 — upstream returned a successful HTTP response that did not match
    /// the selected protocol.
    #[error("upstream returned an invalid response: {message}")]
    UpstreamInvalidResponse {
        /// Internal diagnostic detail. Public HTTP/SSE responses use a fixed
        /// safe message and never expose this value.
        message: String,
    },

    /// 401 / 403 — upstream MCP server demanded authorization. Distinct from
    /// [`Upstream`](Self::Upstream) (a generic 502) because the cloud needs the
    /// real status, the `WWW-Authenticate` challenge, and the parsed required
    /// scope to drive OAuth token refresh (401) and step-up (403).
    #[error("upstream auth required ({status})")]
    UpstreamAuth {
        /// The upstream HTTP status — `401` or `403`.
        status: u16,
        /// The verbatim `WWW-Authenticate` header, when present.
        www_authenticate: Option<String>,
        /// The scope the upstream says is required (403 `insufficient_scope`
        /// only); `None` when not named.
        required_scope: Option<String>,
    },

    /// 504 — upstream timed out.
    #[error("upstream timeout")]
    UpstreamTimeout,

    /// 503 — the upstream route set is temporarily unavailable.
    #[error("upstream unavailable")]
    UpstreamUnavailable,

    /// 500 — internal error.
    #[error("internal error: {0}")]
    Internal(String),
}

fn upstream_payload_text(error: &serde_json::Value) -> String {
    match error {
        serde_json::Value::String(message) => message.clone(),
        other => other.to_string(),
    }
}

impl BitrouterError {
    /// The HTTP status code this error renders as.
    pub fn status(&self) -> u16 {
        match self {
            Self::BadRequest { .. } => 400,
            Self::Unauthorized(_) => 401,
            Self::PaymentRequired(_) => 402,
            Self::UpstreamPaymentRequired => 402,
            Self::Forbidden(_) => 403,
            Self::NotFound(_) => 404,
            Self::RateLimited { .. } => 429,
            Self::UpstreamRateLimited { .. } => 429,
            Self::UpstreamBadRequest { .. } => 400,
            Self::Upstream { .. } => 502,
            Self::UpstreamInvalidResponse { .. } => 502,
            Self::UpstreamAuth { status, .. } => *status,
            Self::UpstreamTimeout => 504,
            Self::UpstreamUnavailable => 503,
            Self::Internal(_) => 500,
        }
    }

    /// The OpenAI-compatible `error.type` tag.
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::BadRequest { .. } => "invalid_request_error",
            Self::Unauthorized(_) => "authentication_error",
            Self::PaymentRequired(_) => "payment_required",
            Self::UpstreamPaymentRequired => "payment_required",
            Self::Forbidden(_) => "permission_error",
            Self::NotFound(_) => "not_found_error",
            Self::RateLimited { .. } => "rate_limit_error",
            Self::UpstreamRateLimited { .. } => "rate_limit_error",
            Self::UpstreamBadRequest { .. } => "invalid_request_error",
            Self::Upstream { .. }
            | Self::UpstreamInvalidResponse { .. }
            | Self::UpstreamTimeout => "upstream_error",
            Self::UpstreamUnavailable => "upstream_error",
            Self::UpstreamAuth { status: 403, .. } => "permission_error",
            Self::UpstreamAuth { .. } => "authentication_error",
            Self::Internal(_) => "internal_error",
        }
    }

    /// Stable machine-readable code for OpenAI-compatible error envelopes.
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::BadRequest { .. } => "invalid_request",
            Self::Unauthorized(_) => "authentication_error",
            Self::PaymentRequired(_) => "payment_required",
            Self::UpstreamPaymentRequired => "upstream_payment_required",
            Self::Forbidden(_) => "permission_denied",
            Self::NotFound(_) => "not_found",
            Self::RateLimited { .. } => "rate_limit_exceeded",
            Self::UpstreamRateLimited { .. } => "upstream_rate_limited",
            Self::UpstreamBadRequest { .. } => "invalid_request",
            Self::Upstream { .. } => "upstream_bad_gateway",
            Self::UpstreamInvalidResponse { .. } => "upstream_invalid_response",
            Self::UpstreamAuth { .. } => "upstream_auth_required",
            Self::UpstreamTimeout => "upstream_timeout",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::Internal(_) => "internal_error",
        }
    }

    /// Convenience constructor for [`BitrouterError::BadRequest`].
    pub fn bad_request(message: impl fmt::Display) -> Self {
        Self::BadRequest {
            message: message.to_string(),
        }
    }

    /// Convenience constructor for [`BitrouterError::Internal`].
    pub fn internal(message: impl fmt::Display) -> Self {
        Self::Internal(message.to_string())
    }

    /// The machine-readable [`ErrorKind`] for this error.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::BadRequest { .. } => ErrorKind::BadRequest,
            Self::Unauthorized(_) => ErrorKind::Unauthorized,
            Self::PaymentRequired(_) => ErrorKind::PaymentRequired,
            Self::UpstreamPaymentRequired => ErrorKind::UpstreamPaymentRequired,
            Self::Forbidden(_) => ErrorKind::Forbidden,
            Self::NotFound(_) => ErrorKind::NotFound,
            Self::RateLimited { .. } => ErrorKind::RateLimited,
            Self::UpstreamRateLimited { .. } => ErrorKind::UpstreamRateLimited,
            Self::UpstreamBadRequest { .. } => ErrorKind::BadRequest,
            Self::Upstream { .. } => ErrorKind::Upstream,
            Self::UpstreamInvalidResponse { .. } => ErrorKind::UpstreamInvalidResponse,
            Self::UpstreamAuth { .. } => ErrorKind::UpstreamAuth,
            Self::UpstreamTimeout => ErrorKind::UpstreamTimeout,
            Self::UpstreamUnavailable => ErrorKind::UpstreamUnavailable,
            Self::Internal(_) => ErrorKind::Internal,
        }
    }

    /// Project this error into the canonical serializable [`ErrorEnvelope`]
    /// (kind + detail message). Callers that carry additional context layers or
    /// a remediation hint populate [`ErrorBody::context`] / [`ErrorBody::hint`]
    /// themselves — this base projection leaves them empty.
    pub fn to_envelope(&self) -> ErrorEnvelope {
        let message = match self {
            Self::BadRequest { message } => message.clone(),
            Self::Unauthorized(m)
            | Self::PaymentRequired(m)
            | Self::Forbidden(m)
            | Self::NotFound(m)
            | Self::Internal(m) => m.clone(),
            Self::RateLimited { .. } => "rate limited".to_string(),
            Self::UpstreamPaymentRequired => "upstream payment required".to_string(),
            Self::UpstreamRateLimited { .. } => "upstream rate limited".to_string(),
            Self::UpstreamBadRequest { error } => upstream_payload_text(error),
            Self::Upstream { status, message } => {
                format!("upstream error ({status}): {message}")
            }
            Self::UpstreamInvalidResponse { message } => {
                format!("upstream returned an invalid response: {message}")
            }
            Self::UpstreamAuth { status, .. } => {
                format!("upstream auth required ({status})")
            }
            Self::UpstreamTimeout => "upstream timeout".to_string(),
            Self::UpstreamUnavailable => "upstream unavailable".to_string(),
        };
        ErrorEnvelope {
            error: ErrorBody {
                kind: self.kind(),
                message,
                context: Vec::new(),
                hint: None,
            },
        }
    }

    /// A message safe to return to an untrusted HTTP/SSE client.
    ///
    /// Provider bad-request payloads are intentionally preserved so callers can
    /// correct rejected parameters. Other upstream diagnostics may contain
    /// echoed prompts, credentials, provider stack traces, or account details,
    /// so they expose only a stable summary at the public boundary.
    pub fn public_message(&self) -> String {
        match self {
            Self::UpstreamBadRequest { error } => upstream_payload_text(error),
            Self::Upstream { .. } => "upstream request failed".to_string(),
            Self::UpstreamInvalidResponse { .. } => {
                "upstream returned an invalid response".to_string()
            }
            _ => self.to_string(),
        }
    }
}

/// A machine-readable error category — the stable taxonomy shared across typed
/// error consumers. This is a many-to-one projection of [`BitrouterError`];
/// for example, provider and local bad requests both map to [`Self::BadRequest`].
/// Serialized in `snake_case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// 400 — malformed request.
    BadRequest,
    /// 401 — missing/invalid credentials.
    Unauthorized,
    /// 402 — payment required.
    PaymentRequired,
    /// 402 — insufficient credit at every usable upstream route.
    UpstreamPaymentRequired,
    /// 403 — policy violation.
    Forbidden,
    /// 404 — no route / resource not found.
    NotFound,
    /// 429 — rate limited.
    RateLimited,
    /// 429 — all usable upstream routes are rate limited.
    UpstreamRateLimited,
    /// 502 — upstream provider error.
    Upstream,
    /// 502 — malformed success response from an upstream protocol.
    UpstreamInvalidResponse,
    /// 401/403 — upstream demanded authorization.
    UpstreamAuth,
    /// 504 — upstream timed out.
    UpstreamTimeout,
    /// 503 — the upstream route set is temporarily unavailable.
    UpstreamUnavailable,
    /// 500 — internal error.
    Internal,
}

/// The canonical, serializable error body: a stable [`ErrorKind`], a
/// human-readable `message`, optional `context` layers (outermost → innermost),
/// and an optional remediation `hint`. Empty `context` and an absent `hint` are
/// omitted from the JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Machine-readable category.
    pub kind: ErrorKind,
    /// Human-readable root-cause detail.
    pub message: String,
    /// Context layers, outermost first. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<String>,
    /// Remediation hint, when one is recognised. Omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The top-level typed error envelope emitted on stdout by the CLI. Wraps a
/// single [`ErrorBody`] under an `error` key:
/// `{"error": {"kind": …, "message": …}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    /// The error detail.
    pub error: ErrorBody,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_bad_request_error_contract() {
        let error = BitrouterError::UpstreamBadRequest {
            error: serde_json::json!("provider secret"),
        };

        assert_eq!(error.status(), 400);
        assert_eq!(error.error_type(), "invalid_request_error");
        assert_eq!(error.error_code(), "invalid_request");
        assert_eq!(error.kind(), ErrorKind::BadRequest);
        assert_eq!(
            serde_json::to_value(error.kind()).unwrap(),
            serde_json::json!("bad_request")
        );
        assert_eq!(error.public_message(), "provider secret");
        assert!(
            error
                .to_envelope()
                .error
                .message
                .contains("provider secret")
        );
    }

    #[test]
    fn upstream_auth_status_and_type() {
        let unauth = BitrouterError::UpstreamAuth {
            status: 401,
            www_authenticate: Some("Bearer resource_metadata=\"https://x/.well-known\"".into()),
            required_scope: None,
        };
        assert_eq!(unauth.status(), 401);
        assert_eq!(unauth.error_type(), "authentication_error");

        let insufficient = BitrouterError::UpstreamAuth {
            status: 403,
            www_authenticate: Some(
                "Bearer error=\"insufficient_scope\", scope=\"read:files\"".into(),
            ),
            required_scope: Some("read:files".into()),
        };
        assert_eq!(insufficient.status(), 403);
        assert_eq!(insufficient.error_type(), "permission_error");
    }

    #[test]
    fn maps_to_envelope_kind_and_message() {
        let e = BitrouterError::NotFound("route gpt-9".into());
        let env = e.to_envelope();
        assert_eq!(env.error.kind, ErrorKind::NotFound);
        assert_eq!(env.error.message, "route gpt-9");
        assert!(env.error.context.is_empty());
        assert_eq!(env.error.hint, None);
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"error": {"kind": "not_found", "message": "route gpt-9"}})
        );
    }

    #[test]
    fn envelope_message_strips_status_prefix_for_payload_variants() {
        assert_eq!(
            BitrouterError::bad_request("nope")
                .to_envelope()
                .error
                .message,
            "nope"
        );
        assert_eq!(
            BitrouterError::Upstream {
                status: 502,
                message: "boom".into()
            }
            .to_envelope()
            .error
            .message,
            "upstream error (502): boom"
        );
    }
}
