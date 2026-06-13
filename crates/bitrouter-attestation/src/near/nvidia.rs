//! NVIDIA GPU attestation via NRAS (spec §5.1 step 2).
//!
//! NEAR's model report carries an `nvidia_payload` (a JSON document of GPU
//! evidence). We POST it to NVIDIA's Remote Attestation Service (NRAS), which
//! returns an EAT (Entity Attestation Token, a JWT) signed by NVIDIA. A genuine
//! confidential GPU is proven when:
//! - the EAT **signature** verifies against NVIDIA's key (we never trust the
//!   `verify_signature: false` shortcut the reference verifier leaves as a
//!   TODO — fail-closed, spec §1.5),
//! - the overall result claim `x-nvidia-overall-att-result` is `true`, and
//! - the EAT echoes our per-request `eat_nonce` (replay protection).
//!
//! [`check_nras_eat`] is the offline-testable core (signature + claims given a
//! decoding key); [`post_nras`] is the online POST the daemon makes directly to
//! NVIDIA (Decision 4 — off the untrusted cloud).

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};

use crate::VerifyError;

/// NVIDIA NRAS GPU attestation endpoint.
pub const NRAS_GPU_URL: &str = "https://nras.attestation.nvidia.com/v3/attest/gpu";

/// Outcome of checking an NRAS EAT. Every field must hold for [`Self::passed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NrasVerdict {
    /// EAT signature verified against the supplied NVIDIA key.
    pub signature_verified: bool,
    /// `x-nvidia-overall-att-result == true`.
    pub overall_pass: bool,
    /// `eat_nonce` echoes the per-request nonce.
    pub nonce_matches: bool,
}

impl NrasVerdict {
    /// A fully-failed verdict — the fail-closed default when anything is wrong
    /// (bad signature, malformed token, missing claim).
    pub fn failed() -> Self {
        Self {
            signature_verified: false,
            overall_pass: false,
            nonce_matches: false,
        }
    }

    /// True iff the GPU attestation is genuine, passing, and fresh.
    pub fn passed(&self) -> bool {
        self.signature_verified && self.overall_pass && self.nonce_matches
    }
}

#[derive(serde::Deserialize)]
struct EatClaims {
    #[serde(rename = "x-nvidia-overall-att-result")]
    overall_att_result: Option<bool>,
    eat_nonce: Option<String>,
}

/// The NRAS algorithms we accept, matching the reference verifier's list.
const NRAS_ALGORITHMS: &[Algorithm] = &[
    Algorithm::ES384,
    Algorithm::ES256,
    Algorithm::RS256,
    Algorithm::PS256,
];

/// Extract the platform EAT (a JWT string) from an NRAS response body, which is
/// `[["JWT", "<token>"], { "<gpu>": "<token>", … }]`. The platform token at
/// index 0 carries the overall result; that is what binds the verdict.
fn platform_jwt(response_body: &[u8]) -> Result<String, VerifyError> {
    let v: serde_json::Value =
        serde_json::from_slice(response_body).map_err(|e| VerifyError::Malformed {
            what: "nras response",
            detail: e.to_string(),
        })?;
    let entry = v
        .get(0)
        .and_then(serde_json::Value::as_array)
        .ok_or(VerifyError::Malformed {
            what: "nras response",
            detail: "expected a non-empty token array".to_string(),
        })?;
    if entry.first().and_then(serde_json::Value::as_str) != Some("JWT") {
        return Err(VerifyError::Malformed {
            what: "nras response",
            detail: "platform token is not in [\"JWT\", …] form".to_string(),
        });
    }
    entry
        .get(1)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or(VerifyError::Malformed {
            what: "nras response",
            detail: "platform token missing the JWT string".to_string(),
        })
}

/// Verify an NRAS response: EAT signature against `decoding_key`, the overall
/// result claim, and that the EAT nonce echoes `nonce`. Fails closed (returns a
/// failed verdict) on a malformed body or a bad signature, never an error —
/// the caller folds the verdict straight into [`crate::AttestationChecks`].
///
/// Freshness is bound by the per-request `nonce`, not the token `exp`, so `exp`
/// is not enforced here (a cached/replayed token fails the nonce check).
pub fn check_nras_eat(
    response_body: &[u8],
    nonce: &str,
    decoding_key: &DecodingKey,
) -> NrasVerdict {
    let Ok(jwt) = platform_jwt(response_body) else {
        return NrasVerdict::failed();
    };

    // Pick the verification algorithm from the token header, constrained to our
    // allowlist. (jsonwebtoken keys off `algorithms[0]`, so a fixed multi-alg
    // list whose head differs from the header alg errors `InvalidAlgorithm`.)
    let Ok(header) = decode_header(&jwt) else {
        return NrasVerdict::failed();
    };
    if !NRAS_ALGORITHMS.contains(&header.alg) {
        return NrasVerdict::failed();
    }
    let mut validation = Validation::new(header.alg);
    validation.algorithms = vec![header.alg];
    validation.validate_exp = false;
    validation.validate_aud = false;
    validation.required_spec_claims.clear();

    let Ok(token) = decode::<EatClaims>(&jwt, decoding_key, &validation) else {
        // Signature or structural failure — fail closed.
        return NrasVerdict::failed();
    };

    let overall_pass = token.claims.overall_att_result == Some(true);
    let nonce_matches = token
        .claims
        .eat_nonce
        .as_deref()
        .is_some_and(|n| n.eq_ignore_ascii_case(nonce));

    NrasVerdict {
        signature_verified: true,
        overall_pass,
        nonce_matches,
    }
}

/// POST a model's `nvidia_payload` to NRAS and return the raw response body.
/// Online; the daemon calls this **directly** (not through the untrusted
/// cloud). Errors are transport-level; claim/signature checking is
/// [`check_nras_eat`].
pub async fn post_nras(
    http: &reqwest::Client,
    nras_url: &str,
    nvidia_payload: &str,
) -> Result<Vec<u8>, VerifyError> {
    let resp = http
        .post(nras_url)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .body(nvidia_payload.to_string())
        .send()
        .await
        .map_err(|e| VerifyError::Transport {
            what: "nras attestation",
            source: Box::new(e),
        })?
        .error_for_status()
        .map_err(|e| VerifyError::Transport {
            what: "nras attestation",
            source: Box::new(e),
        })?;
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| VerifyError::Transport {
            what: "nras attestation",
            source: Box::new(e),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};

    // Throwaway EC P-256 keypair generated only for this test — NOT a
    // credential. Stands in for NVIDIA's NRAS signing key so the full ES256
    // signature-verification path runs offline.
    const TEST_EC_PRIVATE_PKCS8_PEM: &str =
        include_str!("../../tests/fixtures/nras_test_ec_private_pkcs8.pem");
    const TEST_EC_PUBLIC_PEM: &str = include_str!("../../tests/fixtures/nras_test_ec_public.pem");

    const NONCE: &str = "9a01356cb451dc2c3c0ce9a195245a0be984a3f73617f55f87913fc2f059cba7";

    fn signing_key() -> EncodingKey {
        EncodingKey::from_ec_pem(TEST_EC_PRIVATE_PKCS8_PEM.as_bytes()).expect("test priv key")
    }

    fn verifying_key() -> DecodingKey {
        DecodingKey::from_ec_pem(TEST_EC_PUBLIC_PEM.as_bytes()).expect("test pub key")
    }

    /// Build an NRAS-shaped response body whose platform EAT carries the given
    /// result + nonce, signed with the test key.
    fn nras_body(overall: bool, eat_nonce: &str) -> Vec<u8> {
        let claims = serde_json::json!({
            "x-nvidia-overall-att-result": overall,
            "eat_nonce": eat_nonce,
        });
        let jwt = encode(&Header::new(Algorithm::ES256), &claims, &signing_key()).unwrap();
        serde_json::to_vec(&serde_json::json!([["JWT", jwt], {}])).unwrap()
    }

    #[test]
    fn accepts_a_passing_signed_eat_with_matching_nonce() {
        let body = nras_body(true, NONCE);
        let verdict = check_nras_eat(&body, NONCE, &verifying_key());
        assert!(verdict.passed());
        assert!(verdict.signature_verified && verdict.overall_pass && verdict.nonce_matches);
    }

    #[test]
    fn rejects_a_failing_result_claim() {
        let body = nras_body(false, NONCE);
        let verdict = check_nras_eat(&body, NONCE, &verifying_key());
        assert!(verdict.signature_verified);
        assert!(!verdict.overall_pass);
        assert!(!verdict.passed());
    }

    #[test]
    fn rejects_a_replayed_nonce() {
        let body = nras_body(true, "00000000000000000000000000000000");
        let verdict = check_nras_eat(&body, NONCE, &verifying_key());
        assert!(!verdict.nonce_matches);
        assert!(!verdict.passed());
    }

    #[test]
    fn rejects_a_token_signed_by_the_wrong_key() {
        // Verify a genuine-looking token against a *different* key: signature
        // fails, so the whole verdict fails closed.
        let body = nras_body(true, NONCE);
        let other = DecodingKey::from_secret(b"not the nvidia key");
        let verdict = check_nras_eat(&body, NONCE, &other);
        assert!(!verdict.signature_verified);
        assert!(!verdict.passed());
    }

    #[test]
    fn rejects_a_malformed_response_body() {
        let verdict = check_nras_eat(b"[\"not jwt shaped\"]", NONCE, &verifying_key());
        assert_eq!(verdict, NrasVerdict::failed());
    }
}
