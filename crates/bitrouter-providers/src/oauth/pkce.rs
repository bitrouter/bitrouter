//! PKCE (Proof Key for Code Exchange) — RFC 7636.
//!
//! Spec: <https://www.rfc-editor.org/rfc/rfc7636>.
//!
//! PKCE binds an OAuth authorization-code redemption to a per-flow secret
//! that the public OAuth client mints up front:
//!
//! 1. Pick a random `code_verifier`: 43–128 characters from the unreserved
//!    URL alphabet (`[A-Z][a-z][0-9]-._~`).
//! 2. Derive a `code_challenge`: base64url-no-padding(SHA-256(verifier)).
//! 3. Send `code_challenge` + `code_challenge_method=S256` on the
//!    `/authorize` redirect, then `code_verifier` on the `/token` exchange.
//!    The server checks `base64url(SHA-256(code_verifier)) == code_challenge`.
//!
//! This module produces verifier+challenge pairs the rest of the
//! [`auth_code`](super::auth_code) flow consumes. RFC 7636 §7.1 recommends
//! a 32-byte verifier source — what [`generate`] uses.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// One PKCE pair: the secret the client keeps (`code_verifier`) and the
/// derived value the authorization server sees up front (`code_challenge`).
/// The challenge method is always `S256` — RFC 7636 §4.2 says servers SHOULD
/// reject `plain` when both methods are supported.
#[derive(Debug, Clone)]
pub struct PkcePair {
    /// The `code_verifier`. 43 base64url characters from 32 CSPRNG bytes —
    /// inside the 43..=128 range RFC 7636 §4.1 mandates.
    pub verifier: String,
    /// The `code_challenge` — base64url-no-padding(SHA-256(verifier)).
    pub challenge: String,
}

/// PKCE challenge method. Always `S256` here — `plain` is unsupported
/// because it offers no protection against a `code_verifier` leak.
pub const CHALLENGE_METHOD: &str = "S256";

/// Mint a fresh PKCE pair using the OS CSPRNG. The verifier is 43
/// base64url characters (the minimum RFC 7636 allows) — long enough to
/// give 256 bits of entropy without bloating the URL the user pastes.
pub fn generate() -> PkcePair {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = derive_challenge(&verifier);
    PkcePair {
        verifier,
        challenge,
    }
}

/// Derive the `code_challenge` from a `code_verifier`. Exposed for tests
/// that need to assert the S256 transform against a known vector.
pub fn derive_challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hash)
}

/// Mint a random `state` parameter to bind the authorize redirect to the
/// callback we accept. RFC 6749 §10.12 calls this out for CSRF protection.
/// 32 CSPRNG bytes → 43 base64url chars; ample.
pub fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 Appendix B — the worked example everyone implementing PKCE
    /// uses as a sanity check.
    #[test]
    fn rfc7636_appendix_b_s256_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let want_challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(derive_challenge(verifier), want_challenge);
    }

    #[test]
    fn generated_pair_is_consistent() {
        let pair = generate();
        assert_eq!(derive_challenge(&pair.verifier), pair.challenge);
    }

    #[test]
    fn verifier_uses_only_unreserved_url_chars() {
        // RFC 7636 §4.1: `code_verifier = high-entropy cryptographic random
        // STRING using the unreserved characters [A-Z] / [a-z] / [0-9] /
        // "-" / "." / "_" / "~"`. base64url-no-padding produces exactly
        // these — the alphabet is `[A-Z][a-z][0-9]-_` (no `+`, no `/`),
        // and we strip padding (`=`). Verify on a generated sample.
        let pair = generate();
        for c in pair.verifier.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "unexpected verifier char {c:?}"
            );
        }
    }

    #[test]
    fn verifier_is_within_rfc_length_range() {
        let pair = generate();
        // 32 bytes → 43 base64url chars (no padding). RFC 7636 §4.1 allows
        // 43..=128.
        assert_eq!(pair.verifier.len(), 43);
    }

    #[test]
    fn state_is_43_url_safe_chars() {
        let state = generate_state();
        assert_eq!(state.len(), 43);
        for c in state.chars() {
            assert!(c.is_ascii_alphanumeric() || c == '-' || c == '_');
        }
    }

    #[test]
    fn successive_pairs_differ() {
        // Sanity check on the CSPRNG path — two consecutive calls must not
        // collide (probability of a real collision is negligible).
        let a = generate();
        let b = generate();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }
}
