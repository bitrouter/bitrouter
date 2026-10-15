//! Persisted attempt observations shared by gateway accounting and learning.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionHop {
    pub provider: String,
    pub model: String,
    pub account: Option<String>,
    pub protocol: String,
    pub status: String,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Completed,
    Failed,
    ClientDisconnected,
}
