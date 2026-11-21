//! Content digests for local configuration and installed skill sources.
use sha2::{Digest, Sha256};

pub(crate) fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
