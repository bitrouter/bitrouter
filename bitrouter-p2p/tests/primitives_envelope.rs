use bitrouter_p2p::primitives::types::{
    TYPE_PAYMENT_RECEIPT, TYPE_REGISTRY_NODE, TYPE_REGISTRY_TOMBSTONE,
};
use bitrouter_p2p::primitives::{
    PrimitiveError, PrimitiveErrorKind, SIGNING_DOMAIN, SignedEnvelope, SigningKeyPair,
    assert_no_inline_signature, signing_input,
};
use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn require_primitive_err<T>(
    result: Result<T, PrimitiveError>,
    message: &'static str,
) -> Result<PrimitiveError, Box<dyn std::error::Error>> {
    match result {
        Ok(_) => Err(message.into()),
        Err(err) => Ok(err),
    }
}

fn signer(seed: u8) -> SigningKeyPair {
    SigningKeyPair::from_seed([seed; 32])
}

#[test]
fn registry_node_envelope_signs_and_verifies() -> TestResult {
    let signer = signer(7);
    let payload = json!({
        "node_id": signer.identity(),
        "provider_id": signer.identity(),
        "seq": 1,
        "status": "active",
        "valid_until": "2026-07-01T00:00:00Z",
        "endpoints": [],
        "models": []
    });
    let envelope = SignedEnvelope::sign(TYPE_REGISTRY_NODE, &payload, &signer)?;
    envelope.verify_ed25519_jcs(TYPE_REGISTRY_NODE, Some(&signer.identity()))?;

    let input = signing_input(
        &envelope.type_id,
        &envelope.payload,
        &envelope.proofs[0].protected,
    )?;
    assert!(std::str::from_utf8(&input)?.starts_with(SIGNING_DOMAIN));
    Ok(())
}

#[test]
fn tombstone_and_receipt_envelopes_use_same_primitive_api() -> TestResult {
    let signer = signer(8);
    let tombstone = SignedEnvelope::sign(
        TYPE_REGISTRY_TOMBSTONE,
        &json!({
            "node_id": signer.identity(),
            "provider_id": signer.identity(),
            "seq": 2,
            "reason": "shutdown",
            "effective_at": "2026-07-01T00:00:00Z"
        }),
        &signer,
    )?;
    tombstone.verify_ed25519_jcs(TYPE_REGISTRY_TOMBSTONE, Some(&signer.identity()))?;

    let receipt = SignedEnvelope::sign(
        TYPE_PAYMENT_RECEIPT,
        &json!({
            "challenge_id": "challenge-local-1",
            "provider_id": signer.identity(),
            "amount": {
                "method": "solana",
                "intent": "charge",
                "amount_base_units": "1000"
            }
        }),
        &signer,
    )?;
    receipt.verify_ed25519_jcs(TYPE_PAYMENT_RECEIPT, Some(&signer.identity()))?;
    Ok(())
}

#[test]
fn tampering_is_rejected_with_stable_error_kinds() -> TestResult {
    let keypair = signer(9);
    let payload = json!({"provider_id": keypair.identity(), "seq": 1});
    let envelope = SignedEnvelope::sign(TYPE_REGISTRY_NODE, &payload, &keypair)?;

    let mut tampered_payload = envelope.clone();
    tampered_payload.payload["seq"] = json!(2);
    let err = require_primitive_err(
        tampered_payload.verify_ed25519_jcs(TYPE_REGISTRY_NODE, Some(&keypair.identity())),
        "tampered payload must fail",
    )?;
    assert_eq!(err.kind(), PrimitiveErrorKind::PayloadHashMismatch);

    let mut tampered_type = envelope.clone();
    tampered_type.proofs[0].protected.payload_type = "bitrouter/other/0".to_string();
    let err = require_primitive_err(
        tampered_type.verify_ed25519_jcs(TYPE_REGISTRY_NODE, Some(&keypair.identity())),
        "tampered protected payload type must fail",
    )?;
    assert_eq!(err.kind(), PrimitiveErrorKind::PayloadTypeMismatch);

    let wrong_signer = signer(10);
    let err = require_primitive_err(
        envelope.verify_ed25519_jcs(TYPE_REGISTRY_NODE, Some(&wrong_signer.identity())),
        "wrong expected signer must fail",
    )?;
    assert_eq!(err.kind(), PrimitiveErrorKind::UnexpectedSigner);

    let mut bad_signature = envelope.clone();
    bad_signature.proofs[0].signature = wrong_signer.sign(b"wrong message");
    let err = require_primitive_err(
        bad_signature.verify_ed25519_jcs(TYPE_REGISTRY_NODE, Some(&keypair.identity())),
        "bad signature must fail",
    )?;
    assert_eq!(err.kind(), PrimitiveErrorKind::SignatureVerification);
    Ok(())
}

#[test]
fn inline_signature_payload_lint_rejects_legacy_shapes() -> TestResult {
    let err = require_primitive_err(
        assert_no_inline_signature(&json!({
            "provider_id": "ed25519:abc",
            "nested": { "sig": "legacy" }
        })),
        "inline signature field must fail",
    )?;
    assert_eq!(err.kind(), PrimitiveErrorKind::InlineSignature);
    Ok(())
}
