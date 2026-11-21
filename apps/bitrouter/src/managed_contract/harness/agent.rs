//! Model selection retained in checkpoint bindings.

use serde::{Deserialize, Serialize};

/// Select a fixed model or the configured routing Policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMode {
    #[default]
    Fixed,
    Policy,
}
