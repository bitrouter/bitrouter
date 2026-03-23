//! JWT signing and verification for the BitRouter protocol.
//!
//! Supports two web3 wallet signing schemes:
//!
//! - **SOL_EDDSA** — Solana-style Ed25519 over raw message bytes.
//! - **EIP191K** — EVM-style EIP-191 prefixed secp256k1 ECDSA.
//!
//! Token format: `base64url(header).base64url(claims).base64url(signature)`

use alloy_primitives::Signature as EvmSignature;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use solana_signature::Signature as SolanaSignature;

use crate::auth::JwtError;
use crate::auth::chain::{Caip10, JwtAlgorithm};
use crate::auth::claims::BitrouterClaims;
use crate::auth::keys::MasterKeypair;

/// Sign a set of claims into a JWT string using the master keypair.
///
/// The algorithm and signing method are determined by the chain in the claims:
/// - Solana → SOL_EDDSA (Ed25519 over raw message)
/// - EVM → EIP191K (EIP-191 prefixed secp256k1 ECDSA)
pub fn sign(claims: &BitrouterClaims, keypair: &MasterKeypair) -> Result<String, JwtError> {
    let caip10 = Caip10::parse(&claims.iss)?;
    let alg = caip10.chain.jwt_algorithm();

    // Reject tokens where the explicit `chain` field contradicts `iss`.
    let expected_chain = caip10.chain.caip2();
    if claims.chain != expected_chain {
        return Err(JwtError::Verification(format!(
            "chain mismatch: claims.chain is {}, iss implies {}",
            claims.chain, expected_chain
        )));
    }

    let header_b64 = URL_SAFE_NO_PAD.encode(alg.header_json().as_bytes());
    let payload = serde_json::to_vec(claims).map_err(|e| JwtError::Signing(e.to_string()))?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(&payload);

    let message = format!("{header_b64}.{payload_b64}");

    let sig_bytes = match alg {
        JwtAlgorithm::SolEdDsa => keypair.sign_ed25519(message.as_bytes()),
        JwtAlgorithm::Eip191K => keypair.sign_eip191(message.as_bytes())?,
    };

    let sig_b64 = URL_SAFE_NO_PAD.encode(&sig_bytes);
    Ok(format!("{message}.{sig_b64}"))
}

/// Verify a JWT string and extract the claims.
///
/// Determines the algorithm from the JWT header, then:
/// - SOL_EDDSA: extracts the base58 pubkey from CAIP-10 `iss`, verifies Ed25519.
/// - EIP191K: recovers the EVM address from the EIP-191 signature, compares
///   with the address in CAIP-10 `iss`.
pub fn verify(token: &str) -> Result<BitrouterClaims, JwtError> {
    let (message, sig_b64) = token
        .rsplit_once('.')
        .ok_or_else(|| JwtError::MalformedToken("expected header.payload.signature".into()))?;

    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|e| JwtError::MalformedToken(format!("bad signature encoding: {e}")))?;

    // Decode claims (unverified) to determine chain.
    let (_, payload_b64) = message
        .split_once('.')
        .ok_or_else(|| JwtError::MalformedToken("expected header.payload".into()))?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| JwtError::MalformedToken(format!("bad payload encoding: {e}")))?;
    let claims: BitrouterClaims =
        serde_json::from_slice(&payload).map_err(|e| JwtError::MalformedToken(e.to_string()))?;

    // Parse algorithm from header.
    let alg = decode_algorithm(message)?;

    // Parse CAIP-10 identity from iss.
    let caip10 = Caip10::parse(&claims.iss)?;

    // Verify the algorithm matches the chain.
    let expected_alg = caip10.chain.jwt_algorithm();
    if alg != expected_alg {
        return Err(JwtError::Verification(format!(
            "algorithm mismatch: header says {alg}, chain expects {expected_alg}"
        )));
    }

    // Ensure the CAIP-2 chain in the claims matches the chain implied by iss.
    let expected_chain = caip10.chain.caip2();
    if claims.chain != expected_chain {
        return Err(JwtError::Verification(format!(
            "chain mismatch: claims.chain is {}, iss implies {}",
            claims.chain, expected_chain
        )));
    }

    match alg {
        JwtAlgorithm::SolEdDsa => {
            verify_sol_eddsa(message.as_bytes(), &sig_bytes, &caip10.address)?;
        }
        JwtAlgorithm::Eip191K => {
            verify_eip191k(message.as_bytes(), &sig_bytes, &caip10.address)?;
        }
    }

    Ok(claims)
}

/// Decode a JWT without verifying the signature.
///
/// Used to extract claims before verification (e.g., to read `iss` for
/// account lookup). **Never trust claims from this function without a
/// subsequent `verify()` call.**
pub fn decode_unverified(token: &str) -> Result<BitrouterClaims, JwtError> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(JwtError::MalformedToken(
            "expected exactly 3 segments (header.payload.signature)".into(),
        ));
    }
    let payload_b64 = parts[1];

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
            .map_err(|_| JwtError::Expired)?
            .as_secs();
        if now >= exp {
            return Err(JwtError::Expired);
        }
    }
    Ok(())
}

// ── internal helpers ──────────────────────────────────────────

/// Extract the algorithm from the JWT header segment.
fn decode_algorithm(header_dot_payload: &str) -> Result<JwtAlgorithm, JwtError> {
    let header_b64 = header_dot_payload
        .split_once('.')
        .map(|(h, _)| h)
        .ok_or_else(|| JwtError::MalformedToken("expected header.payload".into()))?;

    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|e| JwtError::MalformedToken(format!("bad header encoding: {e}")))?;

    #[derive(serde::Deserialize)]
    struct Header {
        alg: String,
    }

    let header: Header = serde_json::from_slice(&header_bytes)
        .map_err(|e| JwtError::MalformedToken(format!("bad header JSON: {e}")))?;

    JwtAlgorithm::from_header(&header.alg)
}

/// Verify a SOL_EDDSA (Ed25519) signature.
fn verify_sol_eddsa(message: &[u8], sig_bytes: &[u8], address_b58: &str) -> Result<(), JwtError> {
    let pubkey = crate::auth::keys::decode_solana_pubkey(address_b58)?;

    let sig = SolanaSignature::try_from(sig_bytes)
        .map_err(|_| JwtError::Verification("invalid Ed25519 signature length".into()))?;

    if !sig.verify(pubkey.as_ref(), message) {
        return Err(JwtError::Verification("invalid Ed25519 signature".into()));
    }

    Ok(())
}

/// Verify an EIP191K (EIP-191 + secp256k1) signature.
///
/// Recovers the signer address from the EIP-191 prefixed message and
/// compares it with the expected address from the CAIP-10 `iss`.
fn verify_eip191k(
    message: &[u8],
    sig_bytes: &[u8],
    expected_address: &str,
) -> Result<(), JwtError> {
    let sig = EvmSignature::try_from(sig_bytes)
        .map_err(|_| JwtError::Verification("invalid secp256k1 signature".into()))?;

    let recovered = sig
        .recover_address_from_msg(message)
        .map_err(|e| JwtError::Verification(format!("ecrecover failed: {e}")))?;

    let expected = expected_address
        .parse::<alloy_primitives::Address>()
        .map_err(|e| JwtError::InvalidCaip10(format!("invalid EVM address: {e}")))?;

    if recovered != expected {
        return Err(JwtError::AddressMismatch);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::chain::Chain;
    use crate::auth::claims::TokenScope;
    use crate::auth::keys::MasterKeypair;

    fn test_claims_solana(kp: &MasterKeypair) -> BitrouterClaims {
        let chain = Chain::solana_mainnet();
        let caip10 = kp.caip10(&chain).expect("caip10");
        BitrouterClaims {
            iss: caip10.format(),
            chain: chain.caip2(),
            iat: Some(1_700_000_000),
            exp: None,
            scope: TokenScope::Api,
            models: None,
            tools: None,
            budget: None,
            budget_scope: None,
            budget_range: None,
        }
    }

    fn test_claims_evm(kp: &MasterKeypair) -> BitrouterClaims {
        let chain = Chain::base();
        let caip10 = kp.caip10(&chain).expect("caip10");
        BitrouterClaims {
            iss: caip10.format(),
            chain: chain.caip2(),
            iat: Some(1_700_000_000),
            exp: None,
            scope: TokenScope::Api,
            models: None,
            tools: None,
            budget: None,
            budget_scope: None,
            budget_range: None,
        }
    }

    #[test]
    fn sign_and_verify_solana() {
        let kp = MasterKeypair::generate();
        let claims = test_claims_solana(&kp);
        let token = sign(&claims, &kp).expect("sign");
        let decoded = verify(&token).expect("verify");
        assert_eq!(decoded.iss, claims.iss);
        assert_eq!(decoded.scope, TokenScope::Api);
    }

    #[test]
    fn sign_and_verify_evm() {
        let kp = MasterKeypair::generate();
        let claims = test_claims_evm(&kp);
        let token = sign(&claims, &kp).expect("sign");
        let decoded = verify(&token).expect("verify");
        assert_eq!(decoded.iss, claims.iss);
        assert_eq!(decoded.scope, TokenScope::Api);
    }

    #[test]
    fn verify_rejects_wrong_key_solana() {
        let kp1 = MasterKeypair::generate();
        let kp2 = MasterKeypair::generate();
        let claims = test_claims_solana(&kp1);
        let token = sign(&claims, &kp1).expect("sign");

        // Tamper: replace iss with kp2's address but keep kp1's signature.
        let claims2 = test_claims_solana(&kp2);
        let parts: Vec<&str> = token.split('.').collect();
        let new_payload_b64 =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims2).expect("ser").as_slice());
        let tampered = format!("{}.{}.{}", parts[0], new_payload_b64, parts[2]);
        assert!(verify(&tampered).is_err());
    }

    #[test]
    fn verify_rejects_wrong_key_evm() {
        let kp1 = MasterKeypair::generate();
        let kp2 = MasterKeypair::generate();
        let claims = test_claims_evm(&kp1);
        let token = sign(&claims, &kp1).expect("sign");

        // Tamper: replace iss with kp2's address but keep kp1's signature.
        let claims2 = test_claims_evm(&kp2);
        let parts: Vec<&str> = token.split('.').collect();
        let new_payload_b64 =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims2).expect("ser").as_slice());
        let tampered = format!("{}.{}.{}", parts[0], new_payload_b64, parts[2]);
        assert!(verify(&tampered).is_err());
    }

    #[test]
    fn decode_unverified_extracts_claims() {
        let kp = MasterKeypair::generate();
        let claims = test_claims_solana(&kp);
        let token = sign(&claims, &kp).expect("sign");
        let decoded = decode_unverified(&token).expect("decode");
        assert_eq!(decoded.iss, claims.iss);
        assert_eq!(decoded.chain, claims.chain);
    }

    #[test]
    fn check_expiration_passes_for_future() {
        let claims = BitrouterClaims {
            iss: String::new(),
            chain: String::new(),
            iat: None,
            exp: Some(u64::MAX),
            scope: TokenScope::Api,
            models: None,
            tools: None,
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
            chain: String::new(),
            iat: None,
            exp: Some(1),
            scope: TokenScope::Api,
            models: None,
            tools: None,
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
            chain: String::new(),
            iat: None,
            exp: None,
            scope: TokenScope::Api,
            models: None,
            tools: None,
            budget: None,
            budget_scope: None,
            budget_range: None,
        };
        check_expiration(&claims).expect("no exp means valid");
    }

    #[test]
    fn token_has_three_base64url_parts() {
        let kp = MasterKeypair::generate();
        let claims = test_claims_solana(&kp);
        let token = sign(&claims, &kp).expect("sign");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
    }

    #[test]
    fn solana_header_is_sol_eddsa() {
        let kp = MasterKeypair::generate();
        let claims = test_claims_solana(&kp);
        let token = sign(&claims, &kp).expect("sign");
        let header_b64 = token.split('.').next().expect("header");
        let header = URL_SAFE_NO_PAD.decode(header_b64).expect("decode");
        let header_str = String::from_utf8(header).expect("utf8");
        assert!(header_str.contains("SOL_EDDSA"));
    }

    #[test]
    fn evm_header_is_eip191k() {
        let kp = MasterKeypair::generate();
        let claims = test_claims_evm(&kp);
        let token = sign(&claims, &kp).expect("sign");
        let header_b64 = token.split('.').next().expect("header");
        let header = URL_SAFE_NO_PAD.decode(header_b64).expect("decode");
        let header_str = String::from_utf8(header).expect("utf8");
        assert!(header_str.contains("EIP191K"));
    }

    #[test]
    fn malformed_token_rejected() {
        assert!(decode_unverified("not-a-jwt").is_err());
        assert!(decode_unverified("a.b.c.d").is_err());
    }

    #[test]
    fn sign_rejects_chain_mismatch() {
        let kp = MasterKeypair::generate();
        let sol_chain = Chain::solana_mainnet();
        let caip10 = kp.caip10(&sol_chain).expect("caip10");
        // iss is Solana but chain field claims EVM.
        let bad_claims = BitrouterClaims {
            iss: caip10.format(),
            chain: Chain::base().caip2(),
            iat: None,
            exp: None,
            scope: TokenScope::Api,
            models: None,
            tools: None,
            budget: None,
            budget_scope: None,
            budget_range: None,
        };
        assert!(sign(&bad_claims, &kp).is_err());
    }

    #[test]
    fn verify_rejects_chain_mismatch_in_payload() {
        let kp = MasterKeypair::generate();
        // Sign a valid Solana token, then tamper the chain field in the payload.
        let claims = test_claims_solana(&kp);
        let token = sign(&claims, &kp).expect("sign");

        let parts: Vec<&str> = token.split('.').collect();
        // Replace chain with EVM chain while keeping Solana iss.
        let mut tampered_claims = claims.clone();
        tampered_claims.chain = Chain::base().caip2();
        let new_payload_b64 = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&tampered_claims)
                .expect("ser")
                .as_slice(),
        );
        let tampered = format!("{}.{}.{}", parts[0], new_payload_b64, parts[2]);
        assert!(verify(&tampered).is_err());
    }
}
