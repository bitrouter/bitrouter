//! NEAR per-chat signature verification (spec §5.2; L1.5).
//!
//! For a confidential chat NEAR's TEE signs the text
//! `"{model}:{sha256(request_body)}:{sha256(response_body)}"` with the attested
//! key, as an **EIP-191 `personal_sign`** ECDSA signature (secp256k1). Verifying
//! it means recovering the Ethereum address that signed that exact text and
//! checking it equals the attested `signing_address` — proving the exchange ran
//! in the attested TEE and was not modified in flight.
//!
//! This module owns the crypto: the EIP-191 digest, secp256k1 public-key
//! recovery, and Ethereum address derivation. Binding the recovered address to
//! a policy-accepted attestation happens in [`super::NearVerifier::verify_exchange`].

use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use sha2::{Digest as _, Sha256};
use sha3::Keccak256;

/// NEAR's per-chat signature response (`GET {base}/v1/signature/{chat_id}`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatSignature {
    /// The signed text: `"{model}:{sha256(req)}:{sha256(resp)}"`.
    pub text: String,
    /// 65-byte `r ‖ s ‖ v` signature, hex.
    pub signature: String,
    /// The address the TEE claims signed; must equal the recovered address.
    pub signing_address: String,
    pub signing_algo: String,
}

/// The exact text the TEE signs: `"{model}:{sha256(req)}:{sha256(resp)}"`.
pub fn chat_signing_text(model: &str, request_hash: &str, response_hash: &str) -> String {
    format!("{model}:{request_hash}:{response_hash}")
}

/// `sha256(bytes)` as lowercase hex — the request/response hashes in the text.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The EIP-191 `personal_sign` digest:
/// `keccak256("\x19Ethereum Signed Message:\n" ‖ len(message) ‖ message)`.
fn eip191_digest(message: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(b"\x19Ethereum Signed Message:\n");
    hasher.update(message.len().to_string().as_bytes());
    hasher.update(message);
    hasher.finalize().into()
}

/// The 0x-prefixed, lowercase Ethereum address for a recovered public key:
/// `"0x" ‖ keccak256(uncompressed_pubkey[1..])[12..]`.
fn address_from_verifying_key(vk: &VerifyingKey) -> String {
    let encoded = vk.to_encoded_point(false); // 0x04 ‖ X ‖ Y
    let hash = Keccak256::digest(&encoded.as_bytes()[1..]);
    format!("0x{}", hex::encode(&hash[12..]))
}

/// Normalize a signature `v` byte to a secp256k1 recovery id: accepts the
/// EIP-191 `27`/`28` convention and the raw `0`/`1` form.
fn recovery_id(v: u8) -> Option<RecoveryId> {
    let raw = match v {
        27 | 28 => v - 27,
        0 | 1 => v,
        _ => return None,
    };
    RecoveryId::from_byte(raw)
}

/// Recover the 0x-prefixed Ethereum address that produced `signature_hex` over
/// `message` under EIP-191. `signature_hex` is 65 bytes (`r ‖ s ‖ v`), hex,
/// with or without a `0x` prefix. Returns `None` on any malformed input — the
/// caller folds that into a fail-closed verdict.
pub fn recover_eip191_address(message: &[u8], signature_hex: &str) -> Option<String> {
    let bytes = hex::decode(signature_hex.strip_prefix("0x").unwrap_or(signature_hex)).ok()?;
    if bytes.len() != 65 {
        return None;
    }
    let rec_id = recovery_id(bytes[64])?;
    let signature = Signature::from_slice(&bytes[..64]).ok()?;
    let digest = eip191_digest(message);
    let vk = VerifyingKey::recover_from_prehash(&digest, &signature, rec_id).ok()?;
    Some(address_from_verifying_key(&vk))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::SigningKey;

    /// The canonical Ethereum test key: private key = 1.
    fn privkey_one() -> SigningKey {
        let mut bytes = [0u8; 32];
        bytes[31] = 1;
        SigningKey::from_slice(&bytes).unwrap()
    }

    /// Address for private key = 1 — a widely published known-answer. Validates
    /// the keccak address-derivation against the real Ethereum spec, externally.
    const ADDR_FOR_PRIVKEY_ONE: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";

    fn sign_eip191(key: &SigningKey, message: &[u8]) -> String {
        let digest = eip191_digest(message);
        let (sig, rec_id) = key.sign_prehash_recoverable(&digest).unwrap();
        let mut out = sig.to_bytes().to_vec(); // 64 bytes r ‖ s
        out.push(27 + rec_id.to_byte()); // EIP-191 v
        hex::encode(out)
    }

    #[test]
    fn address_derivation_matches_the_known_answer() {
        let addr = address_from_verifying_key(privkey_one().verifying_key());
        assert_eq!(addr, ADDR_FOR_PRIVKEY_ONE);
    }

    #[test]
    fn recovers_the_signer_of_a_chat_text() {
        let key = privkey_one();
        let text = chat_signing_text("Qwen/Qwen3.5-122B-A10B", &"ab".repeat(32), &"cd".repeat(32));
        let sig = sign_eip191(&key, text.as_bytes());

        let recovered = recover_eip191_address(text.as_bytes(), &sig).expect("recovers");
        assert_eq!(recovered, ADDR_FOR_PRIVKEY_ONE);
    }

    #[test]
    fn a_tampered_message_recovers_a_different_address() {
        let key = privkey_one();
        let text = chat_signing_text("m", &"ab".repeat(32), &"cd".repeat(32));
        let sig = sign_eip191(&key, text.as_bytes());

        // Flip the response hash: the signature no longer recovers our address.
        let tampered = chat_signing_text("m", &"ab".repeat(32), &"ce".repeat(32));
        let recovered = recover_eip191_address(tampered.as_bytes(), &sig);
        assert_ne!(recovered.as_deref(), Some(ADDR_FOR_PRIVKEY_ONE));
    }

    #[test]
    fn malformed_signatures_return_none() {
        assert!(recover_eip191_address(b"x", "not-hex").is_none());
        assert!(recover_eip191_address(b"x", &"00".repeat(64)).is_none()); // 64 bytes, need 65
        assert!(recover_eip191_address(b"x", &format!("{}07", "00".repeat(64))).is_none()); // bad v
    }

    #[test]
    fn sha256_hex_and_text_format() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(chat_signing_text("m", "r", "s"), "m:r:s");
    }
}
