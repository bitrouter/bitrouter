//! Error payloads of the control API, independent of HTTP transport errors.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

// A remote peer can put arbitrary diagnostic material in either string.
redacted_contract_debug!(ErrorEnvelope, ErrorBody);
