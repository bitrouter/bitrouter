//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use serde::{Deserialize, Serialize};
pub mod work;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTokenAccounting {
    /// Sum of known configured estimates only. `None` means sum overflow.
    pub known_subtotal_micro_usd: Option<u64>,
    pub known_attempts: u32,
    pub unknown_attempts: u32,
}

impl Default for RunTokenAccounting {
    fn default() -> Self {
        Self {
            known_subtotal_micro_usd: Some(0),
            known_attempts: 0,
            unknown_attempts: 0,
        }
    }
}

impl RunTokenAccounting {
    /// Admitted intents without an acknowledged outcome. This includes uncertain
    /// dispatches; a persisted intent alone never establishes zero provider cost.
    pub fn pending_attempts(&self, admitted_attempts: u32) -> u32 {
        admitted_attempts
            .saturating_sub(self.known_attempts)
            .saturating_sub(self.unknown_attempts)
    }
    /// A complete configured token estimate only when every admitted attempt has
    /// known evidence. This does not establish a complete invoice for the run.
    pub fn complete_estimate_micro_usd(&self, admitted_attempts: u32) -> Option<u64> {
        if self.known_attempts == admitted_attempts && self.unknown_attempts == 0 {
            self.known_subtotal_micro_usd
        } else {
            None
        }
    }
}
