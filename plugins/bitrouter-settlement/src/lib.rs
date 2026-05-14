//! # bitrouter-settlement
//!
//! Settlement plugin. Exports individually-registrable hooks — `ByokRouteHook`,
//! `BalanceCheckHook`, `MppStreamHook`, the `ChargeStrategy` chain
//! (`ByokCharge` / `CreditCharge` / `MppCharge`), `ReceiptRecorder`, and
//! `SqliteMetricsStore` — plus the `SettlementBundle` convenience packaging.
//! See design doc 004 §1.
//!
//! MPP delivers the **Tempo** channel only in v1.0; `mpp-solana` is a
//! placeholder feature, never wired (008 §1.1).

#![forbid(unsafe_code)]

pub mod balance;
pub mod bundle;
pub mod byok;
pub mod charge;
pub mod db;
pub mod events;
pub mod metrics_store;
pub mod mpp;
pub mod pricing;
pub mod recorder;

#[cfg(test)]
mod tests;

pub use balance::BalanceCheckHook;
pub use bundle::SettlementBundle;
pub use byok::{ByokCredential, ByokRouteHook, insert_byok_key};
pub use charge::{ByokCharge, CreditCharge, MppCharge, add_credits, credit_balance};
pub use db::{migrate, migrations};
pub use events::{ByokKeyApplied, MppCheckpointSigned, PricingUnavailable};
pub use metrics_store::SqliteMetricsStore;
pub use mpp::{MppChannel, MppState, MppStreamHook};
pub use pricing::{ModelPricing, PricingTable, calculate_charge_micro_usd};
pub use recorder::ReceiptRecorder;

/// Whether the Tempo MPP channel is compiled in.
pub const MPP_TEMPO_ENABLED: bool = cfg!(feature = "mpp-tempo");
/// Whether the Solana MPP channel is compiled in. v1.0: placeholder, never wired.
pub const MPP_SOLANA_ENABLED: bool = cfg!(feature = "mpp-solana");
