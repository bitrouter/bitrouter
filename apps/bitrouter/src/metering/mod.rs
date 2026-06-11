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
//!    estimated micro-USD from the configured [`PricingTable`] and the
//!    pipeline-observed token counts, then writes a row to the `requests`
//!    table via [`MeteringStore`].
//! 2. [`MeteringStore`] owns the `requests` table — the single writer is
//!    the recorder, the readers are the `policy` and `auth` modules in
//!    the binary (no SDK trait between them, just a concrete type).
//!
//! This module does not charge anyone. The `estimated_charge_micro_usd`
//! column is what we'd bill *if* we were billing — it's read by the
//! policy module's `max_spend_micro_usd` enforcement, surfaced by
//! `bitrouter spend` (future work), and consumed by closed-source
//! billing plugins via their own recorder.

pub mod db;
pub mod entities;
pub mod pricing;
pub mod recorder;
pub mod store;

#[cfg(test)]
mod tests;

pub use db::RequestMetric;
pub use pricing::{ContextTier, ModelPricing, PricingTable, calculate_charge_micro_usd};
pub use recorder::MeteringRecorder;
pub use store::{MeteringStore, RateMetrics, TimeWindow, TokenUsage};
