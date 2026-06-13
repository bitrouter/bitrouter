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

use std::collections::HashMap;

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};

use crate::VerifyError;

/// NVIDIA NRAS GPU attestation endpoint.
pub const NRAS_GPU_URL: &str = "https://nras.attestation.nvidia.com/v3/attest/gpu";

/// NVIDIA's NRAS JWKS endpoint — the rotating set of EAT signing keys.
pub const NVIDIA_NRAS_JWKS_URL: &str = "https://nras.attestation.nvidia.com/.well-known/jwks.json";

/// Resolves the NVIDIA NRAS EAT verification key. Wraps `jsonwebtoken` so
/// callers (the daemon, the CLI, third parties) don't take a direct
/// `jsonwebtoken` dependency. NVIDIA rotates its signing keys, so the right one
/// is selected per request by the EAT's `kid` — use [`Self::fetch_jwks`]
/// ([`NVIDIA_NRAS_JWKS_URL`]) for that. [`Self::from_ec_pem`] pins a single key.
/// Pin/fetch in the trusted process — never through the untrusted cloud (§1.5).
pub struct NvidiaEatKey(KeySource);

enum KeySource {
    /// A single pinned key, used regardless of the EAT `kid`.
    Single(DecodingKey),
    /// NVIDIA's JWKS — resolve the key by the EAT header's `kid`.
    Jwks(HashMap<String, DecodingKey>),
    /// No key configured — the GPU check fails closed.
    Unconfigured,
}

impl NvidiaEatKey {
    /// Pin a single EC public-key PEM (NRAS signs EATs with ES384/ES256). Used
    /// regardless of the EAT `kid` — fragile against NVIDIA's key rotation;
    /// prefer [`Self::fetch_jwks`].
    pub fn from_ec_pem(pem: &[u8]) -> Result<Self, VerifyError> {
        DecodingKey::from_ec_pem(pem)
            .map(|k| Self(KeySource::Single(k)))
            .map_err(|e| VerifyError::Malformed {
                what: "nvidia eat key",
                detail: e.to_string(),
            })
    }

    /// Build a `kid`-keyed resolver from an NVIDIA JWKS document. Keys without a
    /// `kid` or in an unsupported form are skipped; errors if none are usable.
    pub fn from_jwks_json(bytes: &[u8]) -> Result<Self, VerifyError> {
        let set: JwkSet = serde_json::from_slice(bytes).map_err(|e| VerifyError::Malformed {
            what: "nvidia jwks",
            detail: e.to_string(),
        })?;
        let mut map = HashMap::new();
        for jwk in &set.keys {
            if let (Some(kid), Ok(key)) = (jwk.common.key_id.clone(), DecodingKey::from_jwk(jwk)) {
                map.insert(kid, key);
            }
        }
        if map.is_empty() {
            return Err(VerifyError::Malformed {
                what: "nvidia jwks",
                detail: "no usable keys with a kid".to_string(),
            });
        }
        Ok(Self(KeySource::Jwks(map)))
    }

    /// Fetch NVIDIA's JWKS and build a `kid`-keyed resolver. Online; the daemon
    /// calls NVIDIA directly (Decision 4).
    pub async fn fetch_jwks(url: &str) -> Result<Self, VerifyError> {
        let body = reqwest::Client::new()
            .get(url)
            .send()
            .await
            .map_err(|e| VerifyError::Transport {
                what: "nvidia jwks",
                source: Box::new(e),
            })?
            .error_for_status()
            .map_err(|e| VerifyError::Transport {
                what: "nvidia jwks",
                source: Box::new(e),
            })?
            .bytes()
            .await
            .map_err(|e| VerifyError::Transport {
                what: "nvidia jwks",
                source: Box::new(e),
            })?;
        Self::from_jwks_json(&body)
    }

    /// No key configured — every GPU check fails closed (`gpu_nras_pass=false`),
    /// never a silent pass.
    pub fn unconfigured() -> Self {
        Self(KeySource::Unconfigured)
    }

    /// The verification key for an EAT with the given `kid`, or `None` (which
    /// fails the GPU check closed).
    pub(crate) fn resolve(&self, kid: Option<&str>) -> Option<&DecodingKey> {
        match &self.0 {
            KeySource::Single(key) => Some(key),
            KeySource::Jwks(map) => kid.and_then(|k| map.get(k)),
            KeySource::Unconfigured => None,
        }
    }
}

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
pub fn check_nras_eat(response_body: &[u8], nonce: &str, key: &NvidiaEatKey) -> NrasVerdict {
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
    // Resolve NVIDIA's signing key by the EAT `kid` (rotated); `None` fails
    // closed (no key configured, or an unknown kid).
    let Some(decoding_key) = key.resolve(header.kid.as_deref()) else {
        return NrasVerdict::failed();
    };
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

    const TEST_JWKS: &str = include_str!("../../tests/fixtures/nras_test_jwks.json");

    fn signing_key() -> EncodingKey {
        EncodingKey::from_ec_pem(TEST_EC_PRIVATE_PKCS8_PEM.as_bytes()).expect("test priv key")
    }

    /// A single-pinned resolver over the test public key.
    fn pinned_key() -> NvidiaEatKey {
        NvidiaEatKey::from_ec_pem(TEST_EC_PUBLIC_PEM.as_bytes()).expect("test pub key")
    }

    /// Build an NRAS-shaped response body whose platform EAT carries the given
    /// result + nonce, signed with the test key. `kid` sets the JWT header kid.
    fn nras_body_kid(overall: bool, eat_nonce: &str, kid: Option<&str>) -> Vec<u8> {
        let claims = serde_json::json!({
            "x-nvidia-overall-att-result": overall,
            "eat_nonce": eat_nonce,
        });
        let mut header = Header::new(Algorithm::ES256);
        header.kid = kid.map(str::to_string);
        let jwt = encode(&header, &claims, &signing_key()).unwrap();
        serde_json::to_vec(&serde_json::json!([["JWT", jwt], {}])).unwrap()
    }

    fn nras_body(overall: bool, eat_nonce: &str) -> Vec<u8> {
        nras_body_kid(overall, eat_nonce, None)
    }

    #[test]
    fn accepts_a_passing_signed_eat_with_matching_nonce() {
        let body = nras_body(true, NONCE);
        let verdict = check_nras_eat(&body, NONCE, &pinned_key());
        assert!(verdict.passed());
        assert!(verdict.signature_verified && verdict.overall_pass && verdict.nonce_matches);
    }

    #[test]
    fn rejects_a_failing_result_claim() {
        let body = nras_body(false, NONCE);
        let verdict = check_nras_eat(&body, NONCE, &pinned_key());
        assert!(verdict.signature_verified);
        assert!(!verdict.overall_pass);
        assert!(!verdict.passed());
    }

    #[test]
    fn rejects_a_replayed_nonce() {
        let body = nras_body(true, "00000000000000000000000000000000");
        let verdict = check_nras_eat(&body, NONCE, &pinned_key());
        assert!(!verdict.nonce_matches);
        assert!(!verdict.passed());
    }

    #[test]
    fn unconfigured_key_fails_closed() {
        // No key resolves ⇒ the GPU check can never pass.
        let body = nras_body(true, NONCE);
        let verdict = check_nras_eat(&body, NONCE, &NvidiaEatKey::unconfigured());
        assert!(!verdict.signature_verified);
        assert!(!verdict.passed());
    }

    #[test]
    fn jwks_resolves_the_signing_key_by_kid() {
        let jwks = NvidiaEatKey::from_jwks_json(TEST_JWKS.as_bytes()).expect("jwks parses");

        // A token whose kid is in the JWKS verifies...
        let ok = nras_body_kid(true, NONCE, Some("test-kid-1"));
        assert!(check_nras_eat(&ok, NONCE, &jwks).passed());

        // ...but an unknown kid resolves to no key and fails closed.
        let unknown = nras_body_kid(true, NONCE, Some("rotated-away-kid"));
        assert!(!check_nras_eat(&unknown, NONCE, &jwks).signature_verified);
    }

    #[test]
    fn rejects_a_malformed_response_body() {
        let verdict = check_nras_eat(b"[\"not jwt shaped\"]", NONCE, &pinned_key());
        assert_eq!(verdict, NrasVerdict::failed());
    }
}
