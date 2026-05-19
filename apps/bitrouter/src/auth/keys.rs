//! `brvk_` virtual key generation and hashing.
//!
//! v1 **drops JWT API keys entirely**: the only key form is an
//! opaque virtual key, prefix `brvk_`, of which the database stores **only the
//! SHA-256 hash** — never the plaintext secret. The prefix matches
//!  virtual-key scheme except for the `brvk_` prefix.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// The mandatory prefix for every virtual key.
pub const KEY_PREFIX: &str = "brvk_";

/// A freshly minted virtual key: the plaintext secret (shown to the user
/// **once**) and the SHA-256 hash (the only thing persisted).
#[derive(Debug, Clone)]
pub struct GeneratedKey {
    /// The plaintext `brvk_…` secret. Surface this to the user once, then drop.
    pub secret: String,
    /// Hex-encoded SHA-256 of `secret` — the value stored in `api_keys.key_hash`.
    pub hash: String,
}

/// Mint a new virtual key: `brvk_` + 32 bytes of CSPRNG entropy, URL-safe
/// base64 (no padding).
pub fn generate() -> GeneratedKey {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let secret = format!("{KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes));
    let hash = hash_key(&secret);
    GeneratedKey { secret, hash }
}

/// Hex-encoded SHA-256 of a key's plaintext. Used both to store a new key and
/// to look up a presented one.
pub fn hash_key(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)
}

/// Whether a string is shaped like a virtual key (has the `brvk_` prefix and a
/// non-empty body). This is a cheap shape check, **not** validation — the real
/// check is a hash lookup against `api_keys`.
pub fn looks_like_virtual_key(s: &str) -> bool {
    s.strip_prefix(KEY_PREFIX)
        .is_some_and(|body| !body.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_prefixed_and_unique() {
        let a = generate();
        let b = generate();
        assert!(a.secret.starts_with("brvk_"));
        assert!(looks_like_virtual_key(&a.secret));
        assert_ne!(a.secret, b.secret, "keys are random");
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn hash_is_stable_and_matches_generation() {
        let key = generate();
        assert_eq!(hash_key(&key.secret), key.hash);
        // hex SHA-256 is 64 chars
        assert_eq!(key.hash.len(), 64);
    }

    #[test]
    fn non_virtual_keys_are_rejected_by_shape_check() {
        assert!(!looks_like_virtual_key("sk-openai-style"));
        assert!(!looks_like_virtual_key("brvk_")); // prefix only, empty body
        assert!(!looks_like_virtual_key(""));
        // a JWT-shaped token is not a virtual key — v1 has no JWT path
        assert!(!looks_like_virtual_key("eyJhbGc.eyJzdWI.sig"));
    }
}
