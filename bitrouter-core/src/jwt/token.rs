//! JWT signing and verification for the BitRouter protocol.
//!
//! Implements JWT construction manually using `ed25519-dalek` for the actual
//! Ed25519 signing/verification. This avoids compatibility issues between
//! different crypto backends and keeps the implementation simple and auditable.
//!
//! Token format: `base64url(header).base64url(claims).base64url(signature)`
//! where the header is always `{"alg":"EdDSA","typ":"JWT"}`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use crate::jwt::JwtError;
use crate::jwt::claims::BitrouterClaims;

const JWT_HEADER: &str = r#"{"alg":"EdDSA","typ":"JWT"}"#;

/// Sign a set of claims into a JWT string using the given Ed25519 signing key.
pub fn sign(claims: &BitrouterClaims, signing_key: &SigningKey) -> Result<String, JwtError> {
    let header_b64 = URL_SAFE_NO_PAD.encode(JWT_HEADER.as_bytes());
    let payload = serde_json::to_vec(claims).map_err(|e| JwtError::Signing(e.to_string()))?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(&payload);

    let message = format!("{header_b64}.{payload_b64}");
    let signature = signing_key.sign(message.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());

    Ok(format!("{message}.{sig_b64}"))
}

/// Verify a JWT string and extract the claims.
///
/// The caller provides the expected public key (typically resolved from the
/// `iss` claim after an unverified decode). The signature is verified
/// cryptographically — no database interaction occurs here.
pub fn verify(token: &str, verifying_key: &VerifyingKey) -> Result<BitrouterClaims, JwtError> {
    let (message, sig_b64) = token
        .rsplit_once('.')
        .ok_or_else(|| JwtError::MalformedToken("expected header.payload.signature".into()))?;

    // Verify the Ed25519 signature over "header.payload".
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|e| JwtError::MalformedToken(format!("bad signature encoding: {e}")))?;
    let signature = Signature::from_slice(&sig_bytes)
        .map_err(|_| JwtError::Verification("invalid signature length".into()))?;
    verifying_key
        .verify(message.as_bytes(), &signature)
        .map_err(|_| JwtError::Verification("invalid signature".into()))?;

    // Decode the payload.
    let (_, payload_b64) = message
        .split_once('.')
        .ok_or_else(|| JwtError::MalformedToken("expected header.payload".into()))?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| JwtError::MalformedToken(format!("bad payload encoding: {e}")))?;
    serde_json::from_slice(&payload).map_err(|e| JwtError::MalformedToken(e.to_string()))
}

/// Decode a JWT without verifying the signature.
///
/// Used to extract the `iss` claim (public key) before verification, so we
/// know which key to verify against. **Never trust claims from this function
/// without a subsequent `verify()` call.**
pub fn decode_unverified(token: &str) -> Result<BitrouterClaims, JwtError> {
    let mut parts = token.splitn(3, '.');
    let _header = parts
        .next()
        .ok_or_else(|| JwtError::MalformedToken("missing header".into()))?;
    let payload_b64 = parts
        .next()
        .ok_or_else(|| JwtError::MalformedToken("missing payload".into()))?;

    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| JwtError::MalformedToken(format!("bad payload encoding: {e}")))?;
    serde_json::from_slice(&payload).map_err(|e| JwtError::MalformedToken(e.to_string()))
}

/// Check whether a token's `exp` claim has passed.
///
/// Returns `Ok(())` if the token is still valid (or has no `exp`).
/// Returns `Err(JwtError::Expired)` if the token is expired.
pub fn check_expiration(claims: &BitrouterClaims) -> Result<(), JwtError> {
    if let Some(exp) = claims.exp {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if now > exp {
            return Err(JwtError::Expired);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::claims::TokenScope;
    use crate::jwt::keys::MasterKeypair;

    fn test_claims(kp: &MasterKeypair) -> BitrouterClaims {
        BitrouterClaims {
            iss: kp.public_key_b64(),
            iat: Some(1_700_000_000),
            exp: None,
            scope: TokenScope::Api,
            models: None,
            budget: None,
            budget_scope: None,
            budget_range: None,
        }
    }

    #[test]
    fn sign_and_verify() {
        let kp = MasterKeypair::generate();
        let claims = test_claims(&kp);
        let token = sign(&claims, kp.signing_key()).expect("sign");
        let decoded = verify(&token, &kp.verifying_key()).expect("verify");
        assert_eq!(decoded.iss, claims.iss);
        assert_eq!(decoded.scope, TokenScope::Api);
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let kp1 = MasterKeypair::generate();
        let kp2 = MasterKeypair::generate();
        let claims = test_claims(&kp1);
        let token = sign(&claims, kp1.signing_key()).expect("sign");
        let result = verify(&token, &kp2.verifying_key());
        assert!(result.is_err());
    }

    #[test]
    fn decode_unverified_extracts_claims() {
        let kp = MasterKeypair::generate();
        let claims = test_claims(&kp);
        let token = sign(&claims, kp.signing_key()).expect("sign");
        let decoded = decode_unverified(&token).expect("decode");
        assert_eq!(decoded.iss, claims.iss);
    }

    #[test]
    fn check_expiration_passes_for_future() {
        let claims = BitrouterClaims {
            iss: String::new(),
            iat: None,
            exp: Some(u64::MAX),
            scope: TokenScope::Api,
            models: None,
            budget: None,
            budget_scope: None,
            budget_range: None,
        };
        check_expiration(&claims).expect("not expired");
    }

    #[test]
    fn check_expiration_fails_for_past() {
        let claims = BitrouterClaims {
            iss: String::new(),
            iat: None,
            exp: Some(1),
            scope: TokenScope::Api,
            models: None,
            budget: None,
            budget_scope: None,
            budget_range: None,
        };
        assert!(check_expiration(&claims).is_err());
    }

    #[test]
    fn check_expiration_passes_for_none() {
        let claims = BitrouterClaims {
            iss: String::new(),
            iat: None,
            exp: None,
            scope: TokenScope::Api,
            models: None,
            budget: None,
            budget_scope: None,
            budget_range: None,
        };
        check_expiration(&claims).expect("no exp means valid");
    }

    #[test]
    fn token_has_three_base64url_parts() {
        let kp = MasterKeypair::generate();
        let claims = test_claims(&kp);
        let token = sign(&claims, kp.signing_key()).expect("sign");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        // Header should decode to the expected JSON
        let header = URL_SAFE_NO_PAD.decode(parts[0]).expect("decode header");
        assert_eq!(header, JWT_HEADER.as_bytes());
    }

    #[test]
    fn malformed_token_rejected() {
        assert!(decode_unverified("not-a-jwt").is_err());
        assert!(decode_unverified("a.b.c.d").is_err());
    }
}
