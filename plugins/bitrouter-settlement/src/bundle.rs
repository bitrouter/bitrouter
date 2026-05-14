//! `SettlementBundle` — the optional convenience packaging that registers the
//! whole settlement hook set into an `App` in one call.
//!
//! The bundle is **not** the atomic unit — every hook it installs
//! (`ByokRouteHook`, `BalanceCheckHook`, `MppStreamHook`, `ByokCharge`,
//! `CreditCharge`, `MppCharge`, `ReceiptRecorder`) is `pub` and can be
//! registered individually. The bundle just saves you the wiring (003 §2.1).

use std::sync::Arc;

use sqlx::SqlitePool;

use bitrouter_sdk::app::{AppBuilder, Plugin};
use bitrouter_sdk::metrics::MetricsStore;
use bitrouter_sdk::{MigrationItem, PluginId};

use crate::balance::BalanceCheckHook;
use crate::byok::ByokRouteHook;
use crate::charge::{ByokCharge, CreditCharge, MppCharge};
use crate::db;
use crate::metrics_store::SqliteMetricsStore;
use crate::mpp::{MppState, MppStreamHook};
use crate::pricing::PricingTable;
use crate::recorder::ReceiptRecorder;

/// Convenience packaging for the settlement plugin.
pub struct SettlementBundle {
    id: PluginId,
    pool: SqlitePool,
    pricing: PricingTable,
    metrics_store: Arc<dyn MetricsStore>,
    mpp: Option<MppState>,
}

impl SettlementBundle {
    /// Build a settlement bundle.
    ///
    /// - `pool` — the sqlite pool carrying this plugin's tables.
    /// - `pricing` — the `(provider, service_id)` pricing table.
    /// - `mpp` — `Some` to enable the MPP path (Tempo only in v1.0); `None`
    ///   leaves MPP unwired (credits + BYOK only).
    pub fn new(pool: SqlitePool, pricing: PricingTable, mpp: Option<MppState>) -> Self {
        let metrics_store: Arc<dyn MetricsStore> = Arc::new(SqliteMetricsStore::new(pool.clone()));
        Self {
            id: PluginId::new("bitrouter-settlement"),
            pool,
            pricing,
            metrics_store,
            mpp,
        }
    }

    /// The shared `MetricsStore` this bundle installs — hand it to PreRequest
    /// hooks (e.g. `PolicyHook`) that need to read usage.
    pub fn metrics_store(&self) -> Arc<dyn MetricsStore> {
        self.metrics_store.clone()
    }
}

impl Plugin for SettlementBundle {
    fn id(&self) -> &PluginId {
        &self.id
    }

    fn migrations(&self) -> Vec<MigrationItem> {
        db::migrations()
    }

    fn install(&self, app: &mut AppBuilder) {
        let lm = app.language_model_builder();

        // Stage 1 — pre-flight funding gate.
        lm.pre_request_hook(BalanceCheckHook::new(self.pool.clone(), self.mpp.clone()));
        // Stage 2 — BYOK key injection.
        lm.route_hook(ByokRouteHook::new(self.pool.clone()));
        // StreamHook stage — MPP per-checkpoint streaming settlement.
        if let Some(mpp) = &self.mpp {
            lm.stream_hook(MppStreamHook::new(mpp.clone(), self.pricing.clone()));
        }
        // Stage 4a — the ChargeStrategy chain, in first-claim-wins order:
        // BYOK (free) → Credits → MPP.
        lm.charge_strategy(ByokCharge);
        lm.charge_strategy(CreditCharge::new(self.pool.clone(), self.pricing.clone()));
        if let Some(mpp) = &self.mpp {
            lm.charge_strategy(MppCharge::new(mpp.clone(), self.pricing.clone()));
        }
        // Stage 4b — the always-run receipt recorder.
        lm.settlement_recorder(ReceiptRecorder::new(self.metrics_store.clone()));
    }
}
