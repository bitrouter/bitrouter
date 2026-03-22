//! Solana session voucher serialization and Ed25519 signature verification.
//!
//! Implements the signing/verification scheme from the TypeScript SDK's
//! `session/Voucher.ts`: JSON canonicalization (sorted keys) prepended
//! with a domain separator, signed with Ed25519.

use ed25519_dalek::{Signature, VerifyingKey};
use mpp::server::VerificationError;

use super::solana_types::{SignatureType, SignedSolanaSessionVoucher, SolanaSessionVoucher};

/// Domain separator prepended to the canonical JSON before signing.
const DOMAIN_SEPARATOR: &str = "solana-mpp-session-voucher-v1:";

/// Serialize a voucher into the signable message bytes.
///
/// The message is `DOMAIN_SEPARATOR + canonical_json` where canonical JSON
/// has sorted keys and no whitespace (matching the TypeScript SDK's
/// `JSON.stringify(sortedKeys(voucher))`).
pub fn serialize_voucher(voucher: &SolanaSessionVoucher) -> Vec<u8> {
    // Build a sorted-key JSON object manually to match the TS SDK's
    // canonicalization (Object.keys().sort() + JSON.stringify).
    let mut map = serde_json::Map::new();
    map.insert(
        "chainId".into(),
        serde_json::Value::String(voucher.chain_id.clone()),
    );
    map.insert(
        "channelId".into(),
        serde_json::Value::String(voucher.channel_id.clone()),
    );
    map.insert(
        "channelProgram".into(),
        serde_json::Value::String(voucher.channel_program.clone()),
    );
    map.insert(
        "cumulativeAmount".into(),
        serde_json::Value::String(voucher.cumulative_amount.clone()),
    );
    if let Some(ref expires) = voucher.expires_at {
        map.insert(
            "expiresAt".into(),
            serde_json::Value::String(expires.clone()),
        );
    }
    map.insert(
        "meter".into(),
        serde_json::Value::String(voucher.meter.clone()),
    );
    map.insert(
        "payer".into(),
        serde_json::Value::String(voucher.payer.clone()),
    );
    map.insert(
        "recipient".into(),
        serde_json::Value::String(voucher.recipient.clone()),
    );
    map.insert(
        "sequence".into(),
        serde_json::Value::Number(voucher.sequence.into()),
    );
    map.insert(
        "serverNonce".into(),
        serde_json::Value::String(voucher.server_nonce.clone()),
    );
    map.insert(
        "units".into(),
        serde_json::Value::String(voucher.units.clone()),
    );

    let canonical = serde_json::to_string(&serde_json::Value::Object(map))
        .expect("voucher JSON serialization cannot fail");

    let mut message = Vec::with_capacity(DOMAIN_SEPARATOR.len() + canonical.len());
    message.extend_from_slice(DOMAIN_SEPARATOR.as_bytes());
    message.extend_from_slice(canonical.as_bytes());
    message
}

/// Verify the Ed25519 signature on a signed Solana session voucher.
///
/// Supports both `ed25519` and `swig-session` signature types — both use
/// the same Ed25519 verification, differing only in how the key was derived.
pub fn verify_voucher_signature(
    signed: &SignedSolanaSessionVoucher,
) -> Result<bool, VerificationError> {
    // Both ed25519 and swig-session use Ed25519 verification.
    match signed.signature_type {
        SignatureType::Ed25519 | SignatureType::SwigSession => {}
    }

    let message = serialize_voucher(&signed.voucher);

    let sig_bytes = bs58::decode(&signed.signature).into_vec().map_err(|e| {
        VerificationError::invalid_signature(format!("invalid base58 signature: {e}"))
    })?;

    let sig = Signature::from_slice(&sig_bytes).map_err(|e| {
        VerificationError::invalid_signature(format!("invalid signature bytes: {e}"))
    })?;

    let pubkey_bytes = bs58::decode(&signed.signer)
        .into_vec()
        .map_err(|e| VerificationError::invalid_signature(format!("invalid base58 signer: {e}")))?;

    let verifying_key = VerifyingKey::from_bytes(
        pubkey_bytes
            .as_slice()
            .try_into()
            .map_err(|_| VerificationError::invalid_signature("signer must be 32 bytes"))?,
    )
    .map_err(|e| {
        VerificationError::invalid_signature(format!("invalid Ed25519 public key: {e}"))
    })?;

    Ok(verifying_key.verify_strict(&message, &sig).is_ok())
}

/// Parse a voucher from a credential payload's `voucher` field.
///
/// Validates that the signed voucher's `channelId` matches the payload's.
pub fn parse_voucher_from_payload<'a>(
    payload_channel_id: &'a str,
    signed: &'a SignedSolanaSessionVoucher,
) -> Result<&'a SolanaSessionVoucher, VerificationError> {
    if signed.voucher.channel_id != payload_channel_id {
        return Err(VerificationError::invalid_payload(
            "voucher channelId does not match payload channelId",
        ));
    }
    Ok(&signed.voucher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_voucher_deterministic() {
        let voucher = SolanaSessionVoucher {
            chain_id: "solana:mainnet-beta".into(),
            channel_id: "ch1".into(),
            channel_program: "prog1".into(),
            cumulative_amount: "100".into(),
            expires_at: None,
            meter: "token".into(),
            payer: "alice".into(),
            recipient: "bob".into(),
            sequence: 1,
            server_nonce: "nonce".into(),
            units: "10".into(),
        };
        let msg = serialize_voucher(&voucher);
        let msg_str = std::str::from_utf8(&msg).expect("valid utf8");

        // Must start with domain separator.
        assert!(msg_str.starts_with(DOMAIN_SEPARATOR));

        // Keys must be sorted alphabetically.
        let json_part = &msg_str[DOMAIN_SEPARATOR.len()..];
        let parsed: serde_json::Value = serde_json::from_str(json_part).expect("valid json");
        let keys: Vec<&str> = parsed
            .as_object()
            .expect("object")
            .keys()
            .map(|k| k.as_str())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "keys must be sorted");

        // expiresAt should be absent when None.
        assert!(!json_part.contains("expiresAt"));
    }

    #[test]
    fn serialize_voucher_includes_expires_at() {
        let voucher = SolanaSessionVoucher {
            chain_id: "solana:devnet".into(),
            channel_id: "ch2".into(),
            channel_program: "prog2".into(),
            cumulative_amount: "0".into(),
            expires_at: Some("2026-12-31T23:59:59Z".into()),
            meter: "session".into(),
            payer: "alice".into(),
            recipient: "bob".into(),
            sequence: 0,
            server_nonce: "n2".into(),
            units: "0".into(),
        };
        let msg = serialize_voucher(&voucher);
        let msg_str = std::str::from_utf8(&msg).expect("valid utf8");
        assert!(msg_str.contains("expiresAt"));
    }

    #[test]
    fn verify_voucher_rejects_bad_signature() {
        // Use a real keypair so the public key is valid, but provide a wrong signature.
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let signer = bs58::encode(signing_key.verifying_key().as_bytes()).into_string();
        let bad_sig = bs58::encode(&[1u8; 64]).into_string();

        let signed = SignedSolanaSessionVoucher {
            signature: bad_sig,
            signature_type: SignatureType::Ed25519,
            signer,
            voucher: SolanaSessionVoucher {
                chain_id: "solana:mainnet-beta".into(),
                channel_id: "ch".into(),
                channel_program: "prog".into(),
                cumulative_amount: "0".into(),
                expires_at: None,
                meter: "token".into(),
                payer: "alice".into(),
                recipient: "bob".into(),
                sequence: 0,
                server_nonce: "n".into(),
                units: "0".into(),
            },
        };
        let result = verify_voucher_signature(&signed);
        // Should succeed parsing but fail verification.
        assert!(result.is_ok());
        assert!(!result.expect("no error"), "signature should not verify");
    }
}
