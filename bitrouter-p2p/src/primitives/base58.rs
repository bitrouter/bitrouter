use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::error::{PrimitiveError, Result};

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Base58Bytes(Vec<u8>);

impl Base58Bytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(Self(decode_base58btc(s)?))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl fmt::Display for Base58Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&encode_base58btc(&self.0))
    }
}

impl fmt::Debug for Base58Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Base58Bytes({self})")
    }
}

impl Serialize for Base58Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Base58Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

pub fn encode_base58btc(bytes: &[u8]) -> String {
    bs58::encode(bytes).into_string()
}

pub fn decode_base58btc(s: &str) -> Result<Vec<u8>> {
    if s.is_empty() {
        return Err(PrimitiveError::EmptyBase58);
    }
    if s.starts_with('z') {
        return Err(PrimitiveError::UnexpectedPrefix("z"));
    }
    let bytes = bs58::decode(s)
        .into_vec()
        .map_err(|err| PrimitiveError::InvalidBase58(err.to_string()))?;
    if encode_base58btc(&bytes) != s {
        return Err(PrimitiveError::NonCanonicalBase58);
    }
    Ok(bytes)
}

pub fn decode_base58btc_fixed<const N: usize>(s: &str) -> Result<[u8; N]> {
    let bytes = decode_base58btc(s)?;
    if bytes.len() != N {
        return Err(PrimitiveError::WrongLength {
            expected: N,
            actual: bytes.len(),
        });
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes);
    Ok(out)
}
