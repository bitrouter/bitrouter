pub mod base58;
pub mod digest;
pub mod envelope;
pub mod error;
pub mod identity;
pub mod jcs;
pub mod types;

pub use base58::{Base58Bytes, decode_base58btc, decode_base58btc_fixed, encode_base58btc};
pub use digest::{Sha256Digest, canonical_sha256, canonical_sha256_digest};
pub use envelope::{
    Ed25519JcsProof, ProofProtected, SIGNING_DOMAIN, SignedEnvelope, assert_no_inline_signature,
    signing_input,
};
pub use error::{PrimitiveError, PrimitiveErrorKind, Result};
pub use identity::{Ed25519Identity, Ed25519Signature, SigningKeyPair};
