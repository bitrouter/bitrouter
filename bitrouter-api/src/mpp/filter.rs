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
/// On success, extracts an [`MppPaymentContext`] with the receipt and
/// optional management response.
///
/// On failure, rejects with [`MppChallenge`] (no credential → 402)
/// or [`MppVerificationFailed`] (invalid credential → 402).
pub fn mpp_payment_filter(
    state: Arc<MppState>,
) -> impl Filter<Extract = (MppPaymentContext,), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("authorization")
        .and(warp::any().map(move || state.clone()))
        .and_then(verify_payment)
}

async fn verify_payment(
    auth_header: Option<String>,
    state: Arc<MppState>,
) -> Result<MppPaymentContext, warp::Rejection> {
    // Check if the Authorization header contains a Payment credential.
    let auth_value = match auth_header.as_deref() {
        Some(h) if extract_payment_scheme(h).is_some() => h,
        _ => {
            // No payment credential — issue a 402 session challenge.
            let challenge = state
                .session_challenge(
                    "1",
                    mpp::server::SessionChallengeOptions {
                        unit_type: Some("token"),
                        ..Default::default()
                    },
                )
                .map_err(|e| {
                    warp::reject::custom(MppVerificationFailed {
                        message: format!("failed to generate challenge: {e}"),
                    })
                })?;

            let www_authenticate = mpp::format_www_authenticate(&challenge).map_err(|e| {
                warp::reject::custom(MppVerificationFailed {
                    message: format!("failed to format challenge: {e}"),
                })
            })?;

            return Err(warp::reject::custom(MppChallenge { www_authenticate }));
        }
    };

    // Parse the credential from the full Authorization header value.
    let credential = mpp::parse_authorization(auth_value).map_err(|e| {
        warp::reject::custom(MppVerificationFailed {
            message: format!("invalid payment credential: {e}"),
        })
    })?;

    // Verify the session credential.
    let result = state.verify_session(&credential).await.map_err(|e| {
        warp::reject::custom(MppVerificationFailed {
            message: e.to_string(),
        })
    })?;

    Ok(MppPaymentContext {
        receipt: result.receipt,
        management_response: result.management_response,
    })
}
