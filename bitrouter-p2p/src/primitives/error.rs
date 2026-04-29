use thiserror::Error;

pub type Result<T> = std::result::Result<T, PrimitiveError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimitiveErrorKind {
    EmptyBase58,
    InvalidBase58,
    NonCanonicalBase58,
    WrongLength,
    MissingPrefix,
    UnexpectedPrefix,
    InvalidSigningKey,
    JsonCanonicalization,
    JsonSerialization,
    TypeMismatch,
    MissingProof,
    UnexpectedProofType,
    PayloadTypeMismatch,
    PayloadHashMismatch,
    UnexpectedSigner,
    SignatureVerification,
    InlineSignature,
}

#[derive(Debug, Error)]
pub enum PrimitiveError {
    #[error("empty base58btc payload")]
    EmptyBase58,
    #[error("invalid base58btc payload: {0}")]
    InvalidBase58(String),
    #[error("non-canonical base58btc payload")]
    NonCanonicalBase58,
    #[error("wrong byte length: expected {expected}, got {actual}")]
    WrongLength { expected: usize, actual: usize },
    #[error("missing required prefix `{0}`")]
    MissingPrefix(&'static str),
    #[error("unexpected prefix `{0}`")]
    UnexpectedPrefix(&'static str),
    #[error("invalid signing key: {0}")]
    InvalidSigningKey(String),
    #[error("json canonicalization failed: {0}")]
    JsonCanonicalization(String),
    #[error("json serialization failed: {0}")]
    JsonSerialization(#[from] serde_json::Error),
    #[error("unexpected envelope type: expected {expected}, got {actual}")]
    TypeMismatch { expected: String, actual: String },
    #[error("missing proofs")]
    MissingProof,
    #[error("unexpected proof type: {0}")]
    UnexpectedProofType(String),
    #[error("payload_type mismatch: proof {proof}, envelope {envelope}")]
    PayloadTypeMismatch { proof: String, envelope: String },
    #[error("payload_hash mismatch: expected {expected}, got {actual}")]
    PayloadHashMismatch { expected: String, actual: String },
    #[error("unexpected signer: expected {expected}, got {actual}")]
    UnexpectedSigner { expected: String, actual: String },
    #[error("ed25519 signature verification failed")]
    SignatureVerification,
    #[error("payload contains inline signature field `{0}`")]
    InlineSignature(String),
}

impl PrimitiveError {
    pub fn kind(&self) -> PrimitiveErrorKind {
        match self {
            Self::EmptyBase58 => PrimitiveErrorKind::EmptyBase58,
            Self::InvalidBase58(_) => PrimitiveErrorKind::InvalidBase58,
            Self::NonCanonicalBase58 => PrimitiveErrorKind::NonCanonicalBase58,
            Self::WrongLength { .. } => PrimitiveErrorKind::WrongLength,
            Self::MissingPrefix(_) => PrimitiveErrorKind::MissingPrefix,
            Self::UnexpectedPrefix(_) => PrimitiveErrorKind::UnexpectedPrefix,
            Self::InvalidSigningKey(_) => PrimitiveErrorKind::InvalidSigningKey,
            Self::JsonCanonicalization(_) => PrimitiveErrorKind::JsonCanonicalization,
            Self::JsonSerialization(_) => PrimitiveErrorKind::JsonSerialization,
            Self::TypeMismatch { .. } => PrimitiveErrorKind::TypeMismatch,
            Self::MissingProof => PrimitiveErrorKind::MissingProof,
            Self::UnexpectedProofType(_) => PrimitiveErrorKind::UnexpectedProofType,
            Self::PayloadTypeMismatch { .. } => PrimitiveErrorKind::PayloadTypeMismatch,
            Self::PayloadHashMismatch { .. } => PrimitiveErrorKind::PayloadHashMismatch,
            Self::UnexpectedSigner { .. } => PrimitiveErrorKind::UnexpectedSigner,
            Self::SignatureVerification => PrimitiveErrorKind::SignatureVerification,
            Self::InlineSignature(_) => PrimitiveErrorKind::InlineSignature,
        }
    }
}
