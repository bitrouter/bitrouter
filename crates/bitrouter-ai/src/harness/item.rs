//! Public harness item contracts. Execution and storage remain application-owned.

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct LiveActivity {
    #[serde(default)]
    pub item_id: Option<String>,
    pub truncated: bool,
    pub kind: String,
    pub text: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
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

redacted_contract_debug!(LiveActivity, CallRecord);
