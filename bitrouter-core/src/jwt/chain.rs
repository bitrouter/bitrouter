//! Chain identification and CAIP-10 account types for multi-chain JWT auth.
//!
//! Implements [CAIP-2](https://github.com/ChainAgnostic/CAIPs/blob/main/CAIPs/caip-2.md)
//! chain identifiers and [CAIP-10](https://github.com/ChainAgnostic/CAIPs/blob/main/CAIPs/caip-10.md)
//! account identifiers for cross-chain wallet identity.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::jwt::JwtError;

/// Solana mainnet genesis hash prefix (first 32 bytes, base58-encoded).
const SOLANA_MAINNET_REF: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp";

/// A blockchain network, identified by namespace and reference per CAIP-2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "namespace", content = "reference")]
pub enum Chain {
    /// Solana — Ed25519 signing.
    ///
    /// Reference is the genesis hash prefix (e.g. `5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp`
    /// for mainnet).
    #[serde(rename = "solana")]
    Solana { reference: String },

    /// EVM-compatible chain — secp256k1 / EIP-191 signing.
    ///
    /// Reference is the integer chain ID as a string (e.g. `"8453"` for Base).
    #[serde(rename = "eip155")]
    Evm { reference: String },
}

impl Chain {
    /// Solana mainnet.
    pub fn solana_mainnet() -> Self {
        Self::Solana {
            reference: SOLANA_MAINNET_REF.to_string(),
        }
    }

    /// Base (EVM chain ID 8453).
    pub fn base() -> Self {
        Self::Evm {
            reference: "8453".to_string(),
        }
    }

    /// Format as a CAIP-2 chain identifier string.
    ///
    /// Examples: `"solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"`, `"eip155:8453"`.
    pub fn caip2(&self) -> String {
        match self {
            Self::Solana { reference } => format!("solana:{reference}"),
            Self::Evm { reference } => format!("eip155:{reference}"),
        }
    }

    /// Parse a CAIP-2 chain identifier string.
    pub fn from_caip2(s: &str) -> Result<Self, JwtError> {
        let (namespace, reference) = s
            .split_once(':')
            .ok_or_else(|| JwtError::InvalidChain(format!("missing ':' in chain id: {s}")))?;

        match namespace {
            "solana" => Ok(Self::Solana {
                reference: reference.to_string(),
            }),
            "eip155" => Ok(Self::Evm {
                reference: reference.to_string(),
            }),
            other => Err(JwtError::InvalidChain(format!(
                "unsupported namespace: {other}"
            ))),
        }
    }

    /// Returns the CAIP-2 namespace (`"solana"` or `"eip155"`).
    pub fn namespace(&self) -> &str {
        match self {
            Self::Solana { .. } => "solana",
            Self::Evm { .. } => "eip155",
        }
    }

    /// Returns the JWT algorithm for this chain.
    pub fn jwt_algorithm(&self) -> JwtAlgorithm {
        match self {
            Self::Solana { .. } => JwtAlgorithm::SolEdDsa,
            Self::Evm { .. } => JwtAlgorithm::Eip191K,
        }
    }
}

impl fmt::Display for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.caip2())
    }
}

/// A CAIP-10 account identifier: `<chain_id>:<address>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caip10 {
    /// The chain this account lives on.
    pub chain: Chain,
    /// The on-chain address (base58 pubkey for Solana, `0x`-prefixed hex for EVM).
    pub address: String,
}

impl Caip10 {
    /// Format as a full CAIP-10 account identifier string.
    ///
    /// Examples:
    /// - `"solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpb..."`
    /// - `"eip155:8453:0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045"`
    pub fn format(&self) -> String {
        format!("{}:{}", self.chain.caip2(), self.address)
    }

    /// Parse a CAIP-10 account identifier string.
    ///
    /// The format is `<namespace>:<reference>:<address>`. For EVM chains this is
    /// three colon-separated segments; for Solana it is also three.
    pub fn parse(s: &str) -> Result<Self, JwtError> {
        // Split into exactly namespace:reference:address
        let mut parts = s.splitn(3, ':');
        let namespace = parts
            .next()
            .ok_or_else(|| JwtError::InvalidCaip10(format!("empty CAIP-10: {s}")))?;
        let reference = parts
            .next()
            .ok_or_else(|| JwtError::InvalidCaip10(format!("missing reference in CAIP-10: {s}")))?;
        let address = parts
            .next()
            .ok_or_else(|| JwtError::InvalidCaip10(format!("missing address in CAIP-10: {s}")))?;

        let chain = match namespace {
            "solana" => Chain::Solana {
                reference: reference.to_string(),
            },
            "eip155" => Chain::Evm {
                reference: reference.to_string(),
            },
            other => {
                return Err(JwtError::InvalidCaip10(format!(
                    "unsupported namespace: {other}"
                )));
            }
        };

        Ok(Self {
            chain,
            address: address.to_string(),
        })
    }
}

impl fmt::Display for Caip10 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format())
    }
}

/// JWT algorithm identifiers for web3 wallet signing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwtAlgorithm {
    /// Solana Ed25519 signing (raw message bytes).
    SolEdDsa,
    /// EVM EIP-191 prefixed secp256k1 signing.
    Eip191K,
}

impl JwtAlgorithm {
    /// The `"alg"` value for the JWT header.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SolEdDsa => "SOL_EDDSA",
            Self::Eip191K => "EIP191K",
        }
    }

    /// The full JWT header JSON string.
    pub fn header_json(&self) -> String {
        format!(r#"{{"alg":"{}","typ":"JWT"}}"#, self.as_str())
    }

    /// Parse from the `"alg"` value in a JWT header.
    pub fn from_header(s: &str) -> Result<Self, JwtError> {
        match s {
            "SOL_EDDSA" => Ok(Self::SolEdDsa),
            "EIP191K" => Ok(Self::Eip191K),
            other => Err(JwtError::UnsupportedAlgorithm(other.to_string())),
        }
    }
}

impl fmt::Display for JwtAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_solana_mainnet_caip2() {
        let chain = Chain::solana_mainnet();
        assert_eq!(chain.caip2(), "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp");
    }

    #[test]
    fn chain_base_caip2() {
        let chain = Chain::base();
        assert_eq!(chain.caip2(), "eip155:8453");
    }

    #[test]
    fn chain_caip2_roundtrip_solana() {
        let chain = Chain::solana_mainnet();
        let s = chain.caip2();
        let parsed = Chain::from_caip2(&s).expect("parse");
        assert_eq!(parsed, chain);
    }

    #[test]
    fn chain_caip2_roundtrip_evm() {
        let chain = Chain::base();
        let s = chain.caip2();
        let parsed = Chain::from_caip2(&s).expect("parse");
        assert_eq!(parsed, chain);
    }

    #[test]
    fn chain_from_caip2_rejects_unknown_namespace() {
        assert!(Chain::from_caip2("bitcoin:mainnet").is_err());
    }

    #[test]
    fn chain_from_caip2_rejects_missing_colon() {
        assert!(Chain::from_caip2("solana").is_err());
    }

    #[test]
    fn caip10_format_solana() {
        let id = Caip10 {
            chain: Chain::solana_mainnet(),
            address: "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy".to_string(),
        };
        assert_eq!(
            id.format(),
            "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy"
        );
    }

    #[test]
    fn caip10_format_evm() {
        let id = Caip10 {
            chain: Chain::base(),
            address: "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045".to_string(),
        };
        assert_eq!(
            id.format(),
            "eip155:8453:0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045"
        );
    }

    #[test]
    fn caip10_roundtrip_solana() {
        let s =
            "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy";
        let id = Caip10::parse(s).expect("parse");
        assert_eq!(id.format(), s);
    }

    #[test]
    fn caip10_roundtrip_evm() {
        let s = "eip155:8453:0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045";
        let id = Caip10::parse(s).expect("parse");
        assert_eq!(id.format(), s);
    }

    #[test]
    fn caip10_parse_rejects_missing_address() {
        assert!(Caip10::parse("eip155:8453").is_err());
    }

    #[test]
    fn caip10_parse_rejects_empty() {
        assert!(Caip10::parse("").is_err());
    }

    #[test]
    fn jwt_algorithm_from_chain() {
        assert_eq!(
            Chain::solana_mainnet().jwt_algorithm(),
            JwtAlgorithm::SolEdDsa
        );
        assert_eq!(Chain::base().jwt_algorithm(), JwtAlgorithm::Eip191K);
    }

    #[test]
    fn jwt_algorithm_header_json() {
        assert_eq!(
            JwtAlgorithm::SolEdDsa.header_json(),
            r#"{"alg":"SOL_EDDSA","typ":"JWT"}"#
        );
        assert_eq!(
            JwtAlgorithm::Eip191K.header_json(),
            r#"{"alg":"EIP191K","typ":"JWT"}"#
        );
    }

    #[test]
    fn jwt_algorithm_roundtrip() {
        for alg in [JwtAlgorithm::SolEdDsa, JwtAlgorithm::Eip191K] {
            let parsed = JwtAlgorithm::from_header(alg.as_str()).expect("parse");
            assert_eq!(parsed, alg);
        }
    }

    #[test]
    fn jwt_algorithm_rejects_unknown() {
        assert!(JwtAlgorithm::from_header("RS256").is_err());
    }
}
