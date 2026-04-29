use bitrouter_p2p::primitives::{
    Ed25519Identity, Ed25519Signature, PrimitiveError, PrimitiveErrorKind, Sha256Digest,
    SigningKeyPair, canonical_sha256_digest, decode_base58btc, decode_base58btc_fixed,
    encode_base58btc,
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

#[test]
fn base58btc_roundtrip_with_leading_zeroes() -> TestResult {
    let bytes = [0u8, 0, 1, 2, 3, 255];
    let encoded = encode_base58btc(&bytes);
    assert!(encoded.starts_with("11"));
    assert_eq!(decode_base58btc(&encoded)?, bytes);
    Ok(())
}

#[test]
fn base58btc_rejects_invalid_inputs() -> TestResult {
    for input in ["", "z111", "0OIl"] {
        assert!(decode_base58btc(input).is_err(), "{input} should fail");
    }
    Ok(())
}

#[test]
fn fixed_length_decode_reports_wrong_length() -> TestResult {
    let err = require_primitive_err(
        decode_base58btc_fixed::<32>(&encode_base58btc(&[1, 2, 3])),
        "wrong length must fail",
    )?;
    assert_eq!(err.kind(), PrimitiveErrorKind::WrongLength);
    Ok(())
}

#[test]
fn identity_signature_and_digest_wire_formats_are_strict() -> TestResult {
    let signer = SigningKeyPair::from_seed([42; 32]);
    let identity = signer.identity();
    let identity_text = identity.to_string();
    assert!(identity_text.starts_with("ed25519:"));
    assert_eq!(identity_text.parse::<Ed25519Identity>()?, identity);
    assert!("abc".parse::<Ed25519Identity>().is_err());

    let signature = signer.sign(b"hello bitrouter");
    let signature_text = signature.to_string();
    assert!(!signature_text.starts_with("ed25519:"));
    assert_eq!(signature_text.parse::<Ed25519Signature>()?, signature);
    assert!(
        format!("ed25519:{signature_text}")
            .parse::<Ed25519Signature>()
            .is_err()
    );

    let digest = canonical_sha256_digest(&json!({"a": 1}))?;
    let digest_text = digest.to_string();
    assert!(digest_text.starts_with("sha256:"));
    assert_eq!(digest_text.parse::<Sha256Digest>()?, digest);
    assert!(
        format!("sha256:{}", "f".repeat(64))
            .parse::<Sha256Digest>()
            .is_err()
    );
    Ok(())
}

#[test]
fn jcs_hash_is_key_order_stable_and_array_order_sensitive() -> TestResult {
    let left = canonical_sha256_digest(&json!({"b": 2, "a": 1}))?;
    let right = canonical_sha256_digest(&json!({"a": 1, "b": 2}))?;
    assert_eq!(left, right);

    let first = canonical_sha256_digest(&json!({"items": [1, 2, 3]}))?;
    let second = canonical_sha256_digest(&json!({"items": [3, 2, 1]}))?;
    assert_ne!(first, second);
    Ok(())
}
