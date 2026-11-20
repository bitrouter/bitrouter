//! Tool effect outcomes shared by the gateway and BRO.

use serde::{Deserialize, Serialize};

/// Whether a tool completed its effects.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectStatus {
    Completed,
    NotExecuted,
    Unknown,
}
