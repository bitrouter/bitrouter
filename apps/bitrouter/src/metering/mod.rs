//! OSS metering — pipeline-observed cost data captured to local SQLite.
//!
//! The OSS bitrouter binary uses this module as its sole settlement
//! recorder. It is **not** a shared library plugin: a closed cloud product
//! (or any deployment with its own billing pipeline) writes its own
//! `SettlementRecorder` impl against the SDK trait and skips this entirely.
//!
//! What the module does, end-to-end:
//!
//! 1. [`MeteringRecorder`] is a `SettlementRecorder` registered on the LM
//!    pipeline. After every request (success or failure) it computes the
//!    auditable micro-USD evidence from provider usage and the configured
//!    [`PricingTable`], then writes a row to the `requests` table via
//!    [`MeteringStore`]. Missing evidence remains an explicit unknown charge.
//! 2. [`MeteringStore`] owns the `requests` table — the single writer is
//!    the recorder, the readers are the `policy` and `auth` modules in
//!    the binary (no SDK trait between them, just a concrete type).
//!
//! This module does not charge anyone. Consumers must consult the persisted
//! charge status before using the legacy non-null
//! `estimated_charge_micro_usd` column.

pub mod db;
pub mod entities;
pub mod pricing;
pub mod reader;
pub mod recorder;
pub mod store;

#[cfg(test)]
mod tests;

pub use db::RequestMetric;
pub use pricing::{
    ChargeEvidence, ChargeStatus, ContextTier, EffectivePricingRates, ModelPricing, PricingSource,
    PricingTable, calculate_charge_evidence, calculate_charge_micro_usd,
};
pub use recorder::MeteringRecorder;
pub use store::{
    MeteringStore, MeteringUsageRecord, RateMetrics, TimeWindow, TokenUsage, UsagePriceOverride,
};

/// Render micro-USD for the agent-facing cost surfaces (`status --agent`
/// spend recap, the MCP tool-result footer, the `spawn` exit summary):
/// two decimals normally, four when the amount would otherwise round to
/// nothing.
pub fn fmt_usd(micro_usd: u64) -> String {
    let usd = micro_usd as f64 / 1_000_000.0;
    if micro_usd == 0 || usd >= 0.01 {
        format!("${usd:.2}")
    } else {
        format!("${usd:.4}")
    }
}

#[cfg(test)]
mod fmt_tests {
    use super::fmt_usd;

    #[test]
    fn fmt_usd_picks_precision() {
        assert_eq!(fmt_usd(0), "$0.00");
        assert_eq!(fmt_usd(420_000), "$0.42");
        assert_eq!(fmt_usd(3_100_000), "$3.10");
        assert_eq!(fmt_usd(3_200), "$0.0032");
    }
}
