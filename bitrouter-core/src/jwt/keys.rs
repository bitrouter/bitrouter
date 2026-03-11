//! Ed25519 key management for BitRouter JWT authentication.
//!
//! Wraps `ed25519-dalek` to provide key generation, serialization, and a
//! JSON-friendly "master key" format stored at
//! `BITROUTER_HOME/.keys/<pubkey_prefix>/master.json`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

use crate::jwt::JwtError;

/// An Ed25519 master keypair for signing BitRouter JWTs.
///
/// The private key is 64 bytes: 32-byte seed concatenated with 32-byte public key
/// (standard Ed25519 keypair format). The public key alone is 32 bytes.
#[derive(Clone)]
pub struct MasterKeypair {
    signing_key: SigningKey,
}

impl MasterKeypair {
    /// Generate a new random Ed25519 keypair.
    pub fn generate() -> Self {
        Self {
            signing_key: SigningKey::generate(&mut OsRng),
        }
    }

    /// Reconstruct from the 64-byte keypair bytes (seed + public key).
    pub fn from_keypair_bytes(bytes: &[u8; 64]) -> Result<Self, JwtError> {
        let signing_key =
            SigningKey::from_keypair_bytes(bytes).map_err(|_| JwtError::InvalidKeypair)?;
        Ok(Self { signing_key })
    }

    /// Serialize to the 64-byte keypair format (seed + public key).
    pub fn to_keypair_bytes(&self) -> [u8; 64] {
        self.signing_key.to_keypair_bytes()
    }

    /// Return the signing key (for JWT signing).
    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    /// Return the verifying (public) key.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// The 32-byte public key, base64url-encoded (no padding).
    /// This is the value used as the `iss` claim in JWTs.
    pub fn public_key_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.verifying_key().as_bytes())
    }

    /// A short prefix of the public key for display and directory naming.
    /// Returns the first 16 characters of the base64url-encoded public key.
    pub fn public_key_prefix(&self) -> String {
        let b64 = self.public_key_b64();
        b64[..16.min(b64.len())].to_string()
    }

    /// Serialize to the JSON format stored in `master.json`.
    pub fn to_json(&self) -> MasterKeyJson {
        MasterKeyJson {
            algorithm: "eddsa".to_string(),
            secret_key: URL_SAFE_NO_PAD.encode(self.to_keypair_bytes()),
        }
    }

    /// Deserialize from the JSON format stored in `master.json`.
    pub fn from_json(json: &MasterKeyJson) -> Result<Self, JwtError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(&json.secret_key)
            .map_err(|_| JwtError::InvalidKeypair)?;
        let bytes: [u8; 64] = bytes.try_into().map_err(|_| JwtError::InvalidKeypair)?;
        Self::from_keypair_bytes(&bytes)
    }
}

/// Decode a base64url-encoded public key string into a `VerifyingKey`.
pub fn decode_public_key(b64: &str) -> Result<VerifyingKey, JwtError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(b64)
        .map_err(|_| JwtError::InvalidPublicKey)?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| JwtError::InvalidPublicKey)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| JwtError::InvalidPublicKey)
}

/// JSON-serializable format for the master key file.
///
/// Stored at `BITROUTER_HOME/.keys/<pubkey_prefix>/master.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MasterKeyJson {
    /// Algorithm identifier. Always "eddsa" for Ed25519.
    pub algorithm: String,
    /// The 64-byte keypair (seed + public key), base64url-encoded.
    pub secret_key: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_keypair() {
        let kp = MasterKeypair::generate();
        let bytes = kp.to_keypair_bytes();
        let kp2 = MasterKeypair::from_keypair_bytes(&bytes).expect("valid keypair");
        assert_eq!(kp.public_key_b64(), kp2.public_key_b64());
    }

    #[test]
    fn roundtrip_json() {
        let kp = MasterKeypair::generate();
        let json = kp.to_json();
        let kp2 = MasterKeypair::from_json(&json).expect("valid json");
        assert_eq!(kp.public_key_b64(), kp2.public_key_b64());
    }

    #[test]
    fn public_key_prefix_length() {
        let kp = MasterKeypair::generate();
        assert_eq!(kp.public_key_prefix().len(), 16);
    }

    #[test]
    fn decode_public_key_roundtrip() {
        let kp = MasterKeypair::generate();
        let b64 = kp.public_key_b64();
        let vk = decode_public_key(&b64).expect("valid public key");
        assert_eq!(vk, kp.verifying_key());
    }
}
