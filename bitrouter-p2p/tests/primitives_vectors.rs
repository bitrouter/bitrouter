use bitrouter_p2p::primitives::{
    Ed25519Identity, Ed25519Signature, PrimitiveErrorKind, Sha256Digest, SigningKeyPair,
    canonical_sha256_digest, decode_base58btc, decode_base58btc_fixed, encode_base58btc,
};
use serde_json::json;

#[test]
fn base58btc_roundtrip_with_leading_zeroes() {
    let bytes = [0u8, 0, 1, 2, 3, 255];
    let encoded = encode_base58btc(&bytes);
    assert!(encoded.starts_with("11"));
    assert_eq!(decode_base58btc(&encoded).unwrap(), bytes);
}

#[test]
fn base58btc_rejects_invalid_inputs() {
    for input in ["", "z111", "0OIl"] {
        assert!(decode_base58btc(input).is_err(), "{input} should fail");
    }
}

#[test]
fn fixed_length_decode_reports_wrong_length() {
    let err = decode_base58btc_fixed::<32>(&encode_base58btc(&[1, 2, 3])).unwrap_err();
    assert_eq!(err.kind(), PrimitiveErrorKind::WrongLength);
}

#[test]
fn identity_signature_and_digest_wire_formats_are_strict() {
    let signer = SigningKeyPair::from_seed([42; 32]);
    let identity = signer.identity();
    let identity_text = identity.to_string();
    assert!(identity_text.starts_with("ed25519:"));
    assert_eq!(identity_text.parse::<Ed25519Identity>().unwrap(), identity);
    assert!("abc".parse::<Ed25519Identity>().is_err());

    let signature = signer.sign(b"hello bitrouter");
    let signature_text = signature.to_string();
    assert!(!signature_text.starts_with("ed25519:"));
    assert_eq!(
        signature_text.parse::<Ed25519Signature>().unwrap(),
        signature
    );
    assert!(
        format!("ed25519:{signature_text}")
            .parse::<Ed25519Signature>()
            .is_err()
    );

    let digest = canonical_sha256_digest(&json!({"a": 1})).unwrap();
    let digest_text = digest.to_string();
    assert!(digest_text.starts_with("sha256:"));
    assert_eq!(digest_text.parse::<Sha256Digest>().unwrap(), digest);
    assert!(
        format!("sha256:{}", "f".repeat(64))
            .parse::<Sha256Digest>()
            .is_err()
    );
}

#[test]
fn jcs_hash_is_key_order_stable_and_array_order_sensitive() {
    let left = canonical_sha256_digest(&json!({"b": 2, "a": 1})).unwrap();
    let right = canonical_sha256_digest(&json!({"a": 1, "b": 2})).unwrap();
    assert_eq!(left, right);

    let first = canonical_sha256_digest(&json!({"items": [1, 2, 3]})).unwrap();
    let second = canonical_sha256_digest(&json!({"items": [3, 2, 1]})).unwrap();
    assert_ne!(first, second);
}
