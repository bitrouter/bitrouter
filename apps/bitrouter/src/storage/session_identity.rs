//! Persisted native identity shared by capture, request reservations and recovery.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The complete native identity scope; `key()` is only a storage index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub owner: String,
    pub source: String,
    pub native_session_id: String,
}

impl SessionIdentity {
    pub fn key(&self) -> Result<String> {
        ensure!(
            !self.owner.is_empty() && !self.source.is_empty() && !self.native_session_id.is_empty(),
            "native identity fields must not be empty"
        );
        Ok(Sha256::digest(serde_json::to_vec(self)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }
}
