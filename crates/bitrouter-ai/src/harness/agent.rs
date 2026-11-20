//! Tool execution policy shared by the gateway and BRO.

use serde::{Deserialize, Serialize};

/// The tool permissions selected for a turn.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMode {
    #[default]
    Coding,
    ReadOnly,
}
