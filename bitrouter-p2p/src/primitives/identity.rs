use std::fmt;
use std::str::FromStr;

use ed25519_dalek::{SECRET_KEY_LENGTH, Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::base58::{decode_base58btc_fixed, encode_base58btc};
use super::error::{PrimitiveError, Result};

const ED25519_PREFIX: &str = "ed25519:";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ed25519Identity([u8; 32]);

impl Ed25519Identity {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn verify(&self, message: &[u8], signature: &Ed25519Signature) -> Result<()> {
        let key =
            VerifyingKey::from_bytes(&self.0).map_err(|_| PrimitiveError::SignatureVerification)?;
        let signature = Signature::from_bytes(signature.as_bytes());
        key.verify(message, &signature)
            .map_err(|_| PrimitiveError::SignatureVerification)
    }
}

impl fmt::Display for Ed25519Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{ED25519_PREFIX}{}", encode_base58btc(&self.0))
    }
}

impl fmt::Debug for Ed25519Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ed25519Identity({self})")
    }
}

impl FromStr for Ed25519Identity {
    type Err = PrimitiveError;

    fn from_str(s: &str) -> Result<Self> {
        let payload = s
            .strip_prefix(ED25519_PREFIX)
            .ok_or(PrimitiveError::MissingPrefix(ED25519_PREFIX))?;
        Ok(Self(decode_base58btc_fixed::<32>(payload)?))
    }
}

impl Serialize for Ed25519Identity {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Ed25519Identity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Ed25519Signature([u8; 64]);

impl Ed25519Signature {
    pub fn new(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }
}

impl fmt::Display for Ed25519Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&encode_base58btc(&self.0))
    }
}

impl fmt::Debug for Ed25519Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ed25519Signature({self})")
    }
}

impl FromStr for Ed25519Signature {
    type Err = PrimitiveError;

    fn from_str(s: &str) -> Result<Self> {
        if s.starts_with(ED25519_PREFIX) {
            return Err(PrimitiveError::UnexpectedPrefix(ED25519_PREFIX));
        }
        Ok(Self(decode_base58btc_fixed::<64>(s)?))
    }
}

impl Serialize for Ed25519Signature {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Ed25519Signature {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone)]
pub struct SigningKeyPair(SigningKey);

impl SigningKeyPair {
    pub fn from_secret_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != SECRET_KEY_LENGTH {
            return Err(PrimitiveError::WrongLength {
                expected: SECRET_KEY_LENGTH,
                actual: bytes.len(),
            });
        }
        let mut secret = [0u8; SECRET_KEY_LENGTH];
        secret.copy_from_slice(bytes);
        Ok(Self(SigningKey::from_bytes(&secret)))
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&seed))
    }

    pub fn identity(&self) -> Ed25519Identity {
        Ed25519Identity(self.0.verifying_key().to_bytes())
    }

    pub fn sign(&self, message: &[u8]) -> Ed25519Signature {
        let signature: Signature = self.0.sign(message);
        Ed25519Signature(signature.to_bytes())
    }
}
