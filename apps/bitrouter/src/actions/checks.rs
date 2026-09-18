//! Typed, bounded management reports for daemon-owned request checks.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default process-local receipt page size.
pub const DEFAULT_RECEIPT_LIMIT: usize = 50;
/// Largest process-local receipt page exposed through CLI, IPC, or HTTP.
pub const MAX_RECEIPT_LIMIT: usize = 500;

pub fn validate_receipt_limit(limit: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=MAX_RECEIPT_LIMIT).contains(&limit),
        "receipt limit must be between 1 and {MAX_RECEIPT_LIMIT}"
    );
    Ok(())
}

pub fn validate_receipt_lookup(request_id: &str, incarnation: Option<&str>) -> anyhow::Result<()> {
    crate::actions::administration::validate_identifier(request_id)?;
    if let Some(incarnation) = incarnation {
        crate::actions::administration::validate_identifier(incarnation)?;
    }
    Ok(())
}

/// Process-local receipt retention metadata without any receipt rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReceiptRetention {
    /// Identity of the daemon process that owns these receipts.
    pub incarnation_id: String,
    /// Maximum total number of active and completed receipts in memory.
    pub capacity: usize,
    /// Completed-receipt time-to-live.
    pub completed_ttl_secs: u64,
    /// Whether current-process receipt queries are authoritative.
    pub health: bitrouter_sdk::language_model::receipts::RequestReceiptStoreHealth,
}

/// Running checker inventory plus saved/running configuration evidence.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChecksReport {
    /// Inventory comes from the current daemon's running configuration.
    pub resolved_via: String,
    /// Saved/running/restart evidence owned by this daemon. `None` means an
    /// older or limited daemon could not provide it, never that config matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_state: Option<crate::reload::ConfigurationState>,
    /// Current-process receipt scope and retention.
    pub receipt_retention: ReceiptRetention,
    /// Compiled capability registrations and the running router bindings that
    /// currently use them.
    pub checkers: Vec<crate::request_checks::CheckerInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_queries_are_bounded_before_transport() {
        assert!(validate_receipt_limit(1).is_ok());
        assert!(validate_receipt_limit(MAX_RECEIPT_LIMIT).is_ok());
        assert!(validate_receipt_limit(0).is_err());
        assert!(validate_receipt_limit(MAX_RECEIPT_LIMIT + 1).is_err());
        assert!(validate_receipt_lookup("request-1", Some("incarnation-1")).is_ok());
        assert!(validate_receipt_lookup("request\n1", None).is_err());
    }
}
