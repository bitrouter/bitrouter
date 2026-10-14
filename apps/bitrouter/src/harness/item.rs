//! Stable BRO call identities and bounded live Item presentation.
//! SDK Message remains the model-content contract; no second message model is introduced.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveActivity {
    #[serde(default)]
    pub item_id: Option<String>,
    pub truncated: bool,
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CallRecord {
    #[serde(default)]
    pub origin: CallOrigin,
    pub item_id: String,
    pub provider_call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallOrigin {
    #[default]
    Model,
    Verification,
}

/// Shared bound for volatile output and status detail.
pub(crate) const MAX_LIVE_BYTES: usize = 32 * 1024;
