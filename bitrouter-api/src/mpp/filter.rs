use std::sync::Arc;

use warp::Filter;

use mpp::protocol::core::extract_payment_scheme;

use super::state::MppState;

/// Context returned by the MPP payment filter after successful credential verification.
pub struct MppPaymentContext {
    /// The payment receipt to attach to the response.
    pub receipt: mpp::Receipt,
    /// If `Some`, this is a management action (channel open/topUp/close).
    /// The caller should return this JSON body directly instead of
    /// processing the normal request.
    pub management_response: Option<serde_json::Value>,
    /// Channel identifier for post-response deduction (from `receipt.reference`).
    pub channel_id: String,
    /// Backend key matching the backend in [`MppState::backends`] that processed
    /// this credential (used for routing deductions to the correct store).
    pub backend_key: String,
}

/// Rejection: no valid payment credential — the client must pay first.
#[derive(Debug)]
pub struct MppChallenge {
    pub www_authenticate: String,
}

impl warp::reject::Reject for MppChallenge {}

/// Rejection: payment verification failed.
#[derive(Debug)]
pub struct MppVerificationFailed {
    pub message: String,
}

impl warp::reject::Reject for MppVerificationFailed {}

/// Creates a Warp filter that verifies MPP session payments.
///
/// The caller's `chain` (from JWT claims) selects which payment backend
/// to use. If no chain is available, challenges from all backends are
/// returned in the 402 response.
///
/// On success, extracts an [`MppPaymentContext`] with the receipt and
/// optional management response.
///
/// On failure, rejects with [`MppChallenge`] (no credential → 402)
/// or [`MppVerificationFailed`] (invalid credential → 402).
pub fn mpp_payment_filter(
    state: Arc<MppState>,
    chain: Option<String>,
) -> impl Filter<Extract = (MppPaymentContext,), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("authorization")
        .and(warp::any().map(move || state.clone()))
        .and(warp::any().map(move || chain.clone()))
        .and_then(
            |auth_header: Option<String>, state: Arc<MppState>, chain: Option<String>| async move {
                verify_payment_impl(auth_header, &state, chain).await
            },
        )
}

pub(crate) async fn verify_payment_impl(
    auth_header: Option<String>,
    state: &MppState,
    chain: Option<String>,
) -> Result<MppPaymentContext, warp::Rejection> {
    // Check if the Authorization header contains a Payment credential.
    let auth_value = match auth_header.as_deref() {
        Some(h) if extract_payment_scheme(h).is_some() => h,
        _ => {
            // No payment credential — issue a 402 session challenge.

            // Try chain-specific challenge first, fall back to all.
            let challenges = match chain.as_deref() {
                Some(c) => {
                    let opts = mpp::server::SessionChallengeOptions {
                        unit_type: Some("token"),
                        ..Default::default()
                    };
                    match state.session_challenge(Some(c), "1", opts) {
                        Ok(ch) => vec![ch],
                        Err(_) => state.all_session_challenges(
                            "1",
                            mpp::server::SessionChallengeOptions {
                                unit_type: Some("token"),
                                ..Default::default()
                            },
                        ),
                    }
                }
                None => state.all_session_challenges(
                    "1",
                    mpp::server::SessionChallengeOptions {
                        unit_type: Some("token"),
                        ..Default::default()
                    },
                ),
            };

            if challenges.is_empty() {
                return Err(warp::reject::custom(MppVerificationFailed {
                    message: "no MPP backends available".to_string(),
                }));
            }

            // Format all challenges into a single WWW-Authenticate header
            // (comma-separated per RFC 7235).
            let mut parts = Vec::with_capacity(challenges.len());
            for ch in &challenges {
                let formatted = mpp::format_www_authenticate(ch).map_err(|e| {
                    warp::reject::custom(MppVerificationFailed {
                        message: format!("failed to format challenge: {e}"),
                    })
                })?;
                parts.push(formatted);
            }
            let www_authenticate = parts.join(", ");

            return Err(warp::reject::custom(MppChallenge { www_authenticate }));
        }
    };

    // Parse the credential from the full Authorization header value.
    let credential = mpp::parse_authorization(auth_value).map_err(|e| {
        warp::reject::custom(MppVerificationFailed {
            message: format!("invalid payment credential: {e}"),
        })
    })?;

    // Verify the session credential against the matching backend.
    let (backend_key, result) = state.verify_session(&credential).await.map_err(|e| {
        warp::reject::custom(MppVerificationFailed {
            message: e.to_string(),
        })
    })?;

    Ok(MppPaymentContext {
        channel_id: result.receipt.reference.clone(),
        receipt: result.receipt,
        management_response: result.management_response,
        backend_key,
    })
}

/// Standalone MPP payment verification for use inside handler functions.
///
/// Reads the `Authorization` header from the current warp request context
/// and verifies the Payment credential. Returns an [`MppPaymentContext`]
/// on success, or rejects with 402 challenge / verification error.
///
/// This is the handler-level alternative to [`mpp_payment_filter`], used when
/// the chain is only known at handler time (e.g. extracted from JWT claims).
pub async fn verify_mpp_payment(
    state: Arc<MppState>,
    chain: Option<String>,
    auth_header: Option<String>,
) -> Result<MppPaymentContext, warp::Rejection> {
    verify_payment_impl(auth_header, &state, chain).await
}

/// Converts MPP-related warp rejections into proper HTTP 402 responses.
///
/// Call this inside your `.recover()` handler. Returns `Some(response)` if the
/// rejection is an [`MppChallenge`] or [`MppVerificationFailed`], `None`
/// otherwise — allowing callers to fall through to other rejection handling.
///
/// For [`MppChallenge`]:
/// - Status: 402 Payment Required
/// - Header: `WWW-Authenticate: <challenge>`
///
/// For [`MppVerificationFailed`]:
/// - Status: 402 Payment Required
pub fn handle_mpp_rejection(err: &warp::Rejection) -> Option<warp::http::Response<String>> {
    use warp::http::StatusCode;

    if let Some(challenge) = err.find::<MppChallenge>() {
        let body = serde_json::json!({
            "error": {
                "message": "payment required",
                "code": 402
            }
        })
        .to_string();

        let response = warp::http::Response::builder()
            .status(StatusCode::PAYMENT_REQUIRED)
            .header("content-type", "application/json")
            .header("www-authenticate", &challenge.www_authenticate)
            .body(body)
            .ok()?;

        return Some(response);
    }

    if let Some(failed) = err.find::<MppVerificationFailed>() {
        let body = serde_json::json!({
            "error": {
                "message": failed.message,
                "code": 402
            }
        })
        .to_string();

        let response = warp::http::Response::builder()
            .status(StatusCode::PAYMENT_REQUIRED)
            .header("content-type", "application/json")
            .body(body)
            .ok()?;

        return Some(response);
    }

    None
}
