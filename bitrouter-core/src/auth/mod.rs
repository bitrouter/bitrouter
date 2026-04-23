//! BitRouter JWT protocol types.
//!
//! Defines the open JWT claims standard shared between the bitrouter CLI,
//! self-hosted servers, and the BitRouter cloud service. JWTs are signed
//! by the operator's OWS wallet — Ed25519 for Solana (`SOL_EDDSA`) or
//! EIP-191 secp256k1 for EVM chains (`EIP191K`). The server verifies
//! signatures against the configured operator wallet identity.

pub mod access;
pub mod chain;
pub mod claims;
pub mod identity;
pub mod keys;
pub mod revocation;
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
    /// `iss` claim does not parse as any known issuer shape (CAIP-10 or
    /// RFC 7638 JWK thumbprint).
    #[error("invalid issuer: {0}")]
    InvalidIssuer(String),
    /// Header `jwk` is missing on a host-thumbprint token where it is required.
    #[error("missing jwk header on host-thumbprint token")]
    MissingJwk,
    /// Header `jwk` is present but malformed (wrong kty/crv, undecodable `x`).
    #[error("invalid jwk header: {0}")]
    InvalidJwk(String),
    /// SHA-256 thumbprint of header `jwk` does not match the `iss` claim.
    #[error("jwk thumbprint does not match iss")]
    ThumbprintMismatch,
    /// Header `alg` is incompatible with the parsed issuer kind (cross-alg
    /// forgery attempt — e.g. `SOL_EDDSA` with a thumbprint `iss`, or
    /// `EdDSA` with a CAIP-10 `iss`).
    #[error("algorithm {alg} not permitted for {issuer_kind} issuer")]
    AlgIssuerMismatch {
        alg: &'static str,
        issuer_kind: &'static str,
    },
}
