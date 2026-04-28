use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use super::base58::{decode_base58btc_fixed, encode_base58btc};
use super::error::{PrimitiveError, Result};
use super::jcs::canonical_json;

const SHA256_PREFIX: &str = "sha256:";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SHA256_PREFIX}{}", encode_base58btc(&self.0))
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256Digest({self})")
    }
}

impl FromStr for Sha256Digest {
    type Err = PrimitiveError;

    fn from_str(s: &str) -> Result<Self> {
        let payload = s
            .strip_prefix(SHA256_PREFIX)
            .ok_or(PrimitiveError::MissingPrefix(SHA256_PREFIX))?;
        Ok(Self(decode_base58btc_fixed::<32>(payload)?))
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

pub fn canonical_sha256<T: serde::Serialize>(value: &T) -> Result<[u8; 32]> {
    let canonical = canonical_json(value)?;
    let mut hasher = Sha256::new();
    hasher.update(canonical);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

pub fn canonical_sha256_digest<T: serde::Serialize>(value: &T) -> Result<Sha256Digest> {
    Ok(Sha256Digest(canonical_sha256(value)?))
}
