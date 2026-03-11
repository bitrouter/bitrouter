//! BitRouter JWT protocol types.
//!
//! Defines the open JWT claims standard shared between the bitrouter CLI,
//! self-hosted servers, and the BitRouter cloud service. JWTs are self-signed
//! with EdDSA (Ed25519) — users hold the private key, servers store only the
//! public key.

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
    #[error("malformed token: {0}")]
    MalformedToken(String),
    #[error("signing failed: {0}")]
    Signing(String),
    #[error("verification failed: {0}")]
    Verification(String),
    #[error("token expired")]
    Expired,
}
