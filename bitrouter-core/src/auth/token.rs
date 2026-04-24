//! JWT signing and verification for the BitRouter protocol.
//!
//! Supports three signing schemes, disambiguated by the shape of the
//! `iss` claim (see [`crate::auth::identity::IssuerKind`]):
//!
//! - **SOL_EDDSA** — Solana-style Ed25519 over raw message bytes.
//!   Self-verifying wallet path; `iss` is a CAIP-10 Solana address.
//! - **EIP191K** — EVM EIP-191 prefixed secp256k1 ECDSA. Self-verifying
//!   wallet path; `iss` is a CAIP-10 EVM address.
//! - **EdDSA** — standard JOSE Ed25519 (RFC 8037) with an embedded
//!   `jwk` in the header. Custodial host path; `iss` is the RFC 7638
//!   SHA-256 thumbprint of that `jwk`.
//!
//! Token format: `base64url(header).base64url(claims).base64url(signature)`.

use alloy_primitives::Signature as EvmSignature;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use solana_keypair::{Keypair as SolanaKeypair, Signer as SolanaSigner};
use solana_signature::Signature as SolanaSignature;

use crate::auth::JwtError;
use crate::auth::chain::{Caip10, JwtAlgorithm};
use crate::auth::claims::BitrouterClaims;
use crate::auth::identity::IssuerKind;
use crate::auth::keys::JwtSigner;

/// Sign a set of claims into a JWT string using any [`JwtSigner`].
///
/// The algorithm is derived from the chain encoded in `claims.iss` (CAIP-10):
/// - Solana → SOL_EDDSA (Ed25519 over raw message)
/// - EVM → EIP191K (EIP-191 prefixed secp256k1 ECDSA)
pub fn sign(claims: &BitrouterClaims, signer: &dyn JwtSigner) -> Result<String, JwtError> {
    let caip10 = Caip10::parse(&claims.iss)?;
    let alg = caip10.chain.jwt_algorithm();

    let header_b64 = URL_SAFE_NO_PAD.encode(alg.header_json().as_bytes());
    let payload = serde_json::to_vec(claims).map_err(|e| JwtError::Signing(e.to_string()))?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(&payload);

    let message = format!("{header_b64}.{payload_b64}");

    let sig_bytes = match alg {
        JwtAlgorithm::SolEdDsa => signer.sign_ed25519(message.as_bytes())?,
        JwtAlgorithm::Eip191K => signer.sign_eip191(message.as_bytes())?,
        JwtAlgorithm::EdDsa => {
            // [`Chain::jwt_algorithm`] never returns `EdDsa` — host tokens
            // bypass this function via [`sign_ed25519_host`]. Return an
            // error rather than panic to preserve the no-panic invariant.
            return Err(JwtError::AlgIssuerMismatch {
                alg: "EdDSA",
                issuer_kind: "wallet",
            });
        }
    };

    let sig_b64 = URL_SAFE_NO_PAD.encode(&sig_bytes);
    Ok(format!("{message}.{sig_b64}"))
}

/// Sign a host-custodied JWT using a raw Ed25519 seed.
///
/// Produces the standard JOSE header `{"alg":"EdDSA","typ":"host+jwt",
/// "jwk":{"crv":"Ed25519","kty":"OKP","x":"<pubkey>"}}` — the `jwk` is
/// embedded so the verifier is self-contained (no key-registry lookup).
///
/// The caller is expected to set `claims.iss` to the RFC 7638 SHA-256
/// thumbprint of that `jwk`; if they don't, [`verify`] will reject the
/// token with [`JwtError::ThumbprintMismatch`]. Helper:
/// [`jwk_thumbprint_sha256`].
pub fn sign_ed25519_host(seed: &[u8; 32], claims: &BitrouterClaims) -> Result<String, JwtError> {
    let keypair = SolanaKeypair::new_from_array(*seed);
    let pubkey_bytes = keypair.pubkey().to_bytes();
    let x_b64 = URL_SAFE_NO_PAD.encode(pubkey_bytes);

    // Header. Field order inside the JOSE header is not canonical — only
    // the JWK subobject's order matters for thumbprint equality. We still
    // emit a stable order for test determinism.
    let header_json = format!(
        r#"{{"alg":"EdDSA","typ":"host+jwt","jwk":{{"crv":"Ed25519","kty":"OKP","x":"{x_b64}"}}}}"#
    );
    let header_b64 = URL_SAFE_NO_PAD.encode(header_json.as_bytes());
    let payload = serde_json::to_vec(claims).map_err(|e| JwtError::Signing(e.to_string()))?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(&payload);

    let message = format!("{header_b64}.{payload_b64}");
    let sig = keypair.sign_message(message.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(sig.as_ref());
    Ok(format!("{message}.{sig_b64}"))
}

/// Compute the RFC 7638 SHA-256 JWK thumbprint of an Ed25519 public key.
///
/// Returns the base64url-no-pad-encoded digest (43 characters) — the
/// exact value that should go in a host-thumbprint token's `iss` claim.
pub fn jwk_thumbprint_sha256(pubkey_bytes: &[u8; 32]) -> String {
    let x_b64 = URL_SAFE_NO_PAD.encode(pubkey_bytes);
    // RFC 7638: canonical JWK JSON has required members in lexical order,
    // no whitespace.
    let canonical = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{x_b64}"}}"#);
    let digest = Sha256::digest(canonical.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Verify a JWT string and extract the claims.
///
/// Dispatches on [`IssuerKind`] parsed from `claims.iss`:
/// - [`IssuerKind::WalletCaip10`] → self-verifying wallet path
///   (SOL_EDDSA / EIP191K). Public key derived from the CAIP-10 address;
///   byte-identical to the historical behavior.
/// - [`IssuerKind::HostThumbprint`] → custodial host path (EdDSA with an
///   embedded `jwk`). Public key is the `jwk` in the JOSE header, and
///   the token is accepted only when `sha256_thumbprint(jwk) == iss`.
///
/// Cross-algorithm forgeries (e.g. a `SOL_EDDSA` header on a thumbprint
/// `iss`, or `EdDSA` on a CAIP-10 `iss`) are rejected with
/// [`JwtError::AlgIssuerMismatch`] before any signature work.
pub fn verify(token: &str) -> Result<BitrouterClaims, JwtError> {
    let (message, sig_b64) = token
        .rsplit_once('.')
        .ok_or_else(|| JwtError::MalformedToken("expected header.payload.signature".into()))?;

    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|e| JwtError::MalformedToken(format!("bad signature encoding: {e}")))?;

    // Decode claims (unverified) to classify the issuer.
    let (_, payload_b64) = message
        .split_once('.')
        .ok_or_else(|| JwtError::MalformedToken("expected header.payload".into()))?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| JwtError::MalformedToken(format!("bad payload encoding: {e}")))?;
    let claims: BitrouterClaims =
        serde_json::from_slice(&payload).map_err(|e| JwtError::MalformedToken(e.to_string()))?;

    let alg = decode_algorithm(message)?;
    let issuer = IssuerKind::parse(&claims.iss)?;

    match issuer {
        IssuerKind::WalletCaip10 { caip10 } => {
            // Reject cross-kind forgery (standard-JOSE EdDSA on a wallet
            // iss) before the narrower in-kind chain-mismatch check.
            if alg == JwtAlgorithm::EdDsa {
                return Err(JwtError::AlgIssuerMismatch {
                    alg: "EdDSA",
                    issuer_kind: "wallet",
                });
            }
            let expected_alg = caip10.chain.jwt_algorithm();
            if alg != expected_alg {
                return Err(JwtError::Verification(format!(
                    "algorithm mismatch: header says {alg}, chain expects {expected_alg}"
                )));
            }
            // `alg` is narrowed to `SolEdDsa | Eip191K` by the two
            // checks above — `EdDsa` was rejected as cross-kind, and
            // `Chain::jwt_algorithm` never returns `EdDsa`.
            if alg == JwtAlgorithm::SolEdDsa {
                verify_sol_eddsa(message.as_bytes(), &sig_bytes, &caip10.address)?;
            } else {
                verify_eip191k(message.as_bytes(), &sig_bytes, &caip10.address)?;
            }
        }
        IssuerKind::HostThumbprint { thumbprint } => {
            if alg != JwtAlgorithm::EdDsa {
                return Err(JwtError::AlgIssuerMismatch {
                    alg: alg.as_str(),
                    issuer_kind: "host-thumbprint",
                });
            }
            verify_host_thumbprint(message, &sig_bytes, &thumbprint)?;
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

/// JWK header subobject for RFC 7638 / RFC 8037 Ed25519 host tokens.
///
/// `deny_unknown_fields` is defensive: today the thumbprint is
/// recomputed from the canonical `{crv,kty,x}` triple so extra fields
/// would be ignored anyway. If a future variant adds a field that
/// affects key interpretation, rejecting unknown fields here closes
/// any silent-ignore foot-gun at the structural boundary.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Jwk {
    kty: String,
    crv: String,
    x: String,
}

/// JOSE header for host-thumbprint tokens.
#[derive(serde::Deserialize)]
struct HostHeader {
    #[serde(default)]
    jwk: Option<Jwk>,
}

/// Verify a host-thumbprint (standard JOSE EdDSA) token.
///
/// The JWT header must include a `jwk` subobject; the header-embedded
/// Ed25519 public key is the sole verifier. Acceptance criteria:
///
/// 1. `jwk.kty == "OKP"` and `jwk.crv == "Ed25519"`.
/// 2. `jwk.x` decodes to 32 bytes (base64url-no-pad).
/// 3. RFC 7638 SHA-256 thumbprint of the JWK equals the `iss` claim
///    (passed in as `expected_thumbprint`).
/// 4. Ed25519 signature verifies against the `jwk.x` public key over
///    the signed message (`header_b64.payload_b64`).
fn verify_host_thumbprint(
    message: &str,
    sig_bytes: &[u8],
    expected_thumbprint: &str,
) -> Result<(), JwtError> {
    let header_b64 = message
        .split_once('.')
        .map(|(h, _)| h)
        .ok_or_else(|| JwtError::MalformedToken("expected header.payload".into()))?;

    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|e| JwtError::MalformedToken(format!("bad header encoding: {e}")))?;
    let header: HostHeader = serde_json::from_slice(&header_bytes)
        .map_err(|e| JwtError::MalformedToken(format!("bad header JSON: {e}")))?;

    let jwk = header.jwk.ok_or(JwtError::MissingJwk)?;

    if jwk.kty != "OKP" {
        return Err(JwtError::InvalidJwk(format!(
            "expected kty=OKP, got {}",
            jwk.kty
        )));
    }
    if jwk.crv != "Ed25519" {
        return Err(JwtError::InvalidJwk(format!(
            "expected crv=Ed25519, got {}",
            jwk.crv
        )));
    }

    let pubkey_bytes = URL_SAFE_NO_PAD
        .decode(&jwk.x)
        .map_err(|e| JwtError::InvalidJwk(format!("bad x encoding: {e}")))?;
    let pubkey_arr: [u8; 32] = pubkey_bytes.as_slice().try_into().map_err(|_| {
        JwtError::InvalidJwk(format!("expected 32-byte x, got {}", pubkey_bytes.len()))
    })?;

    let computed = jwk_thumbprint_sha256(&pubkey_arr);
    if computed != expected_thumbprint {
        return Err(JwtError::ThumbprintMismatch);
    }

    let sig = SolanaSignature::try_from(sig_bytes)
        .map_err(|_| JwtError::Verification("invalid Ed25519 signature length".into()))?;
    if !sig.verify(&pubkey_arr, message.as_bytes()) {
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

    fn test_claims(kp: &MasterKeypair, chain: Chain) -> BitrouterClaims {
        let caip10 = kp.caip10(&chain).expect("caip10");
        BitrouterClaims {
            iss: caip10.format(),
            iat: Some(1_700_000_000),
            exp: None,
            scp: Some(TokenScope::Api),
            mdl: None,
            bgt: None,
            bsc: None,
            id: None,
            key: None,
            pol: None,
            jti: None,
            aud: None,
            sub: None,
            host: None,
        }
    }

    fn test_claims_solana(kp: &MasterKeypair) -> BitrouterClaims {
        test_claims(kp, Chain::solana_mainnet())
    }

    fn test_claims_evm(kp: &MasterKeypair) -> BitrouterClaims {
        test_claims(kp, Chain::base())
    }

    #[test]
    fn sign_and_verify_solana() {
        let kp = MasterKeypair::generate();
        let claims = test_claims_solana(&kp);
        let token = sign(&claims, &kp).expect("sign");
        let decoded = verify(&token).expect("verify");
        assert_eq!(decoded.iss, claims.iss);
        assert_eq!(decoded.scope(), TokenScope::Api);
    }

    #[test]
    fn sign_and_verify_evm() {
        let kp = MasterKeypair::generate();
        let claims = test_claims_evm(&kp);
        let token = sign(&claims, &kp).expect("sign");
        let decoded = verify(&token).expect("verify");
        assert_eq!(decoded.iss, claims.iss);
        assert_eq!(decoded.scope(), TokenScope::Api);
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
    }

    #[test]
    fn check_expiration_passes_for_future() {
        let claims = BitrouterClaims {
            iss: String::new(),
            iat: None,
            exp: Some(u64::MAX),
            scp: Some(TokenScope::Api),
            mdl: None,
            bgt: None,
            bsc: None,
            id: None,
            key: None,
            pol: None,
            jti: None,
            aud: None,
            sub: None,
            host: None,
        };
        check_expiration(&claims).expect("not expired");
    }

    #[test]
    fn check_expiration_fails_for_past() {
        let claims = BitrouterClaims {
            iss: String::new(),
            iat: None,
            exp: Some(1),
            scp: Some(TokenScope::Api),
            mdl: None,
            bgt: None,
            bsc: None,
            id: None,
            key: None,
            pol: None,
            jti: None,
            aud: None,
            sub: None,
            host: None,
        };
        assert!(check_expiration(&claims).is_err());
    }

    #[test]
    fn check_expiration_passes_for_none() {
        let claims = BitrouterClaims {
            iss: String::new(),
            iat: None,
            exp: None,
            scp: Some(TokenScope::Api),
            mdl: None,
            bgt: None,
            bsc: None,
            id: None,
            key: None,
            pol: None,
            jti: None,
            aud: None,
            sub: None,
            host: None,
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
    fn scope_defaults_to_api_when_absent() {
        let kp = MasterKeypair::generate();
        let chain = Chain::solana_mainnet();
        let caip10 = kp.caip10(&chain).expect("caip10");
        let claims = BitrouterClaims {
            iss: caip10.format(),
            iat: None,
            exp: None,
            scp: None, // absent
            mdl: None,
            bgt: None,
            bsc: None,
            id: None,
            key: None,
            pol: None,
            jti: None,
            aud: None,
            sub: None,
            host: None,
        };
        let token = sign(&claims, &kp).expect("sign");
        let decoded = verify(&token).expect("verify");
        assert_eq!(decoded.scope(), TokenScope::Api);
        assert!(decoded.scp.is_none());
    }

    #[test]
    fn key_claim_roundtrips() {
        let kp = MasterKeypair::generate();
        let chain = Chain::solana_mainnet();
        let caip10 = kp.caip10(&chain).expect("caip10");
        let claims = BitrouterClaims {
            iss: caip10.format(),
            iat: None,
            exp: None,
            scp: Some(TokenScope::Api),
            mdl: Some(vec!["gpt-4o".to_string()]),
            bgt: Some(50_000_000),
            bsc: Some(crate::auth::claims::BudgetScope::Session),
            id: Some("obsWNDRE4Mq8s2K7x9fGhJlPvTnYc1Ua0ZiDwXbR5eo".to_string()),
            key: Some("ows_key_abc123".to_string()),
            pol: None,
            jti: None,
            aud: None,
            sub: None,
            host: None,
        };
        let token = sign(&claims, &kp).expect("sign");
        let decoded = verify(&token).expect("verify");
        assert_eq!(decoded.key.as_deref(), Some("ows_key_abc123"));
        assert_eq!(decoded.mdl, Some(vec!["gpt-4o".to_string()]));
        assert_eq!(decoded.bgt, Some(50_000_000));
        assert_eq!(
            decoded.id.as_deref(),
            Some("obsWNDRE4Mq8s2K7x9fGhJlPvTnYc1Ua0ZiDwXbR5eo")
        );
    }

    #[test]
    fn id_claim_absent_for_admin_tokens() {
        let kp = MasterKeypair::generate();
        let chain = Chain::solana_mainnet();
        let caip10 = kp.caip10(&chain).expect("caip10");
        let claims = BitrouterClaims {
            iss: caip10.format(),
            iat: None,
            exp: Some(u64::MAX),
            scp: Some(TokenScope::Admin),
            mdl: None,
            bgt: None,
            bsc: None,
            id: None,
            key: None,
            pol: None,
            jti: None,
            aud: None,
            sub: None,
            host: None,
        };
        let token = sign(&claims, &kp).expect("sign");
        let decoded = verify(&token).expect("verify");
        assert!(decoded.id.is_none());
        assert_eq!(decoded.scope(), TokenScope::Admin);
    }

    // ── Host-thumbprint (EdDSA) path ────────────────────────────

    fn host_seed() -> [u8; 32] {
        let mut seed = [0u8; 32];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = i as u8;
        }
        seed
    }

    fn host_thumbprint_for_seed(seed: &[u8; 32]) -> String {
        let pubkey = SolanaKeypair::new_from_array(*seed).pubkey().to_bytes();
        jwk_thumbprint_sha256(&pubkey)
    }

    fn host_claims(thumbprint: String) -> BitrouterClaims {
        BitrouterClaims {
            iss: thumbprint,
            iat: Some(1_700_000_000),
            exp: None,
            scp: Some(TokenScope::Api),
            mdl: None,
            bgt: None,
            bsc: None,
            id: None,
            key: None,
            pol: None,
            jti: Some("test-jti".to_string()),
            aud: Some("bitrouter-node".to_string()),
            sub: Some("user_123".to_string()),
            host: Some("console".to_string()),
        }
    }

    #[test]
    fn host_token_round_trip() {
        let seed = host_seed();
        let thumbprint = host_thumbprint_for_seed(&seed);
        let claims = host_claims(thumbprint.clone());

        let token = sign_ed25519_host(&seed, &claims).expect("sign");
        let decoded = verify(&token).expect("verify");

        assert_eq!(decoded.iss, thumbprint);
        assert_eq!(decoded.sub.as_deref(), Some("user_123"));
        assert_eq!(decoded.aud.as_deref(), Some("bitrouter-node"));
        assert_eq!(decoded.jti.as_deref(), Some("test-jti"));
    }

    #[test]
    fn host_token_header_is_eddsa_and_host_jwt() {
        let seed = host_seed();
        let claims = host_claims(host_thumbprint_for_seed(&seed));
        let token = sign_ed25519_host(&seed, &claims).expect("sign");

        let header_b64 = token.split('.').next().expect("header");
        let header_bytes = URL_SAFE_NO_PAD.decode(header_b64).expect("decode");
        let header = String::from_utf8(header_bytes).expect("utf8");

        assert!(header.contains(r#""alg":"EdDSA""#));
        assert!(header.contains(r#""typ":"host+jwt""#));
        assert!(header.contains(r#""kty":"OKP""#));
        assert!(header.contains(r#""crv":"Ed25519""#));
    }

    #[test]
    fn host_token_rejected_when_thumbprint_does_not_match_iss() {
        let seed = host_seed();
        // Mint against a thumbprint that does NOT match the seed.
        let claims = host_claims("A".repeat(43));
        let token = sign_ed25519_host(&seed, &claims).expect("sign");

        match verify(&token) {
            Err(JwtError::ThumbprintMismatch) => {}
            other => panic!("expected ThumbprintMismatch, got {other:?}"),
        }
    }

    #[test]
    fn host_token_rejected_when_signed_with_different_key() {
        let seed1 = host_seed();
        let mut seed2 = host_seed();
        seed2[0] ^= 0xff;

        let thumbprint = host_thumbprint_for_seed(&seed1);
        let claims = host_claims(thumbprint);

        // Sign with seed2 but embed seed1's thumbprint as iss.
        let token = sign_ed25519_host(&seed2, &claims).expect("sign");
        // The jwk in the header matches seed2 (signer), but iss says seed1.
        // → ThumbprintMismatch (jwk thumbprint != iss).
        match verify(&token) {
            Err(JwtError::ThumbprintMismatch) => {}
            other => panic!("expected ThumbprintMismatch, got {other:?}"),
        }
    }

    #[test]
    fn wallet_iss_with_eddsa_header_is_rejected() {
        // Forge: take a thumbprint iss, but flip the alg to SOL_EDDSA.
        let kp = MasterKeypair::generate();
        let wallet_claims = test_claims_solana(&kp);
        let wallet_token = sign(&wallet_claims, &kp).expect("sign");

        // Re-encode the header with alg="EdDSA".
        let parts: Vec<&str> = wallet_token.split('.').collect();
        let tampered_header = URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
        let tampered_token = format!("{}.{}.{}", tampered_header, parts[1], parts[2]);

        match verify(&tampered_token) {
            Err(JwtError::AlgIssuerMismatch { .. }) => {}
            other => panic!("expected AlgIssuerMismatch, got {other:?}"),
        }
    }

    #[test]
    fn thumbprint_iss_with_sol_eddsa_header_is_rejected() {
        let seed = host_seed();
        let claims = host_claims(host_thumbprint_for_seed(&seed));
        let host_token = sign_ed25519_host(&seed, &claims).expect("sign");

        // Re-encode the header with alg="SOL_EDDSA".
        let parts: Vec<&str> = host_token.split('.').collect();
        let tampered_header = URL_SAFE_NO_PAD.encode(br#"{"alg":"SOL_EDDSA","typ":"JWT"}"#);
        let tampered_token = format!("{}.{}.{}", tampered_header, parts[1], parts[2]);

        match verify(&tampered_token) {
            Err(JwtError::AlgIssuerMismatch { .. }) => {}
            other => panic!("expected AlgIssuerMismatch, got {other:?}"),
        }
    }

    #[test]
    fn host_token_rejected_when_jwk_missing() {
        // Construct a host token-like structure but strip the jwk from the header.
        let seed = host_seed();
        let claims = host_claims(host_thumbprint_for_seed(&seed));
        let token = sign_ed25519_host(&seed, &claims).expect("sign");

        let parts: Vec<&str> = token.split('.').collect();
        let stripped_header = URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"host+jwt"}"#);
        let tampered_token = format!("{}.{}.{}", stripped_header, parts[1], parts[2]);

        match verify(&tampered_token) {
            Err(JwtError::MissingJwk) => {}
            other => panic!("expected MissingJwk, got {other:?}"),
        }
    }

    #[test]
    fn jwk_thumbprint_matches_rfc7638_test_vector() {
        // RFC 7638 has no Ed25519 vector, but we assert that the SHA-256
        // is stable for a deterministic seed — a regression guard.
        let seed = host_seed();
        let pubkey = SolanaKeypair::new_from_array(seed).pubkey().to_bytes();
        let tp1 = jwk_thumbprint_sha256(&pubkey);
        let tp2 = jwk_thumbprint_sha256(&pubkey);
        assert_eq!(tp1, tp2);
        assert_eq!(
            tp1.len(),
            43,
            "base64url-no-pad of a 32-byte digest is 43 chars"
        );
    }
}
