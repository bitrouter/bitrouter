//! BitRouter JWT protocol types.
//!
//! Defines the open JWT claims standard shared between the bitrouter CLI,
//! self-hosted servers, and the BitRouter cloud service. JWTs are signed
//! with web3 wallet keys — Ed25519 for Solana (`SOL_EDDSA`) or EIP-191
//! secp256k1 for EVM chains (`EIP191K`). Users hold the private seed,
//! servers verify signatures and resolve accounts by CAIP-10 identity.

pub mod chain;
pub mod claims;
pub mod keys;
pub mod token;

/// Errors arising from JWT operations.
#[derive(Debug, thiserror::Error)]
pub enum JwtError {
    #[error("invalid keypair bytes")]
    InvalidKeypair,
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("invalid secp256k1 key")]
    InvalidSecp256k1Key,
    #[error("malformed token: {0}")]
    MalformedToken(String),
    #[error("signing failed: {0}")]
    Signing(String),
    #[error("verification failed: {0}")]
    Verification(String),
    #[error("token expired")]
    Expired,
    #[error("unsupported algorithm: {0}")]
    UnsupportedAlgorithm(String),
    #[error("invalid CAIP-10 identifier: {0}")]
    InvalidCaip10(String),
    #[error("invalid chain identifier: {0}")]
    InvalidChain(String),
    #[error("recovered address does not match iss")]
    AddressMismatch,
    #[error("secp256k1 error: {0}")]
    Secp256k1(String),
}
