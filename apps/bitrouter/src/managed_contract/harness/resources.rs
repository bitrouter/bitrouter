//! Material identity retained by checkpoint skill bindings.

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterialRef {
    pub material_id: String,
    pub version: String,
    pub sha256: String,
    pub media_type: String,
    pub provenance: String,
}

redacted_contract_debug!(MaterialRef);
