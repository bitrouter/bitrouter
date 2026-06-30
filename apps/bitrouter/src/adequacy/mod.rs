//! Online adequacy ledger — the escalate-sticky learned state behind adaptive
//! policy-table routing.
//!
//! When `policy_table.adequacy` is enabled, the daemon watches every request a
//! *downgrade* produced (the policy table routed a fingerprint to a cheaper tier
//! than its escalation tier) and counts the ones that hard-fail. Once a
//! fingerprint accumulates `escalation_threshold` failures it is **pinned**: the
//! adaptive [`crate::policy_table_router::PolicyTableRouter`] then routes that
//! fingerprint to the escalation tier instead of the cheap one. A pin decays
//! after `pin_cooldown_secs`, so a downgrade that failed transiently is retried
//! later rather than abandoned forever.
//!
//! This is the safety half of the asymmetric routing rule: the ledger never
//! downgrades on its own — it only escalates a downgrade that is failing — so it
//! can only ever make routing *more* conservative than the static table.
//!
//! The read path ([`AdequacyLedger::is_pinned`]) is synchronous and lock-cheap
//! because it runs inside the ingress [`bitrouter_sdk::PromptTransform`]; the
//! write path ([`AdequacyLedger::observe`]) is async because it persists new
//! pins, and runs from the [`observer::AdequacyObserveHook`] after each request.

pub mod observer;
pub mod store;

use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use bitrouter_sdk::config::AdequacyConfig;

use self::store::AdequacyStore;

/// Learned escalation state: which fingerprints are pinned, and the pre-pin
/// failure tally that drives a pin.
pub struct AdequacyLedger {
    state: RwLock<State>,
    /// Persistence for pins. `None` in unit tests / when no db is wired.
    store: Option<AdequacyStore>,
    /// Hard failures on a downgraded fingerprint before it is pinned (>= 1).
    threshold: u32,
    /// Seconds a pin lasts before the downgrade is re-attempted. `0` = no decay.
    cooldown_secs: u64,
}

#[derive(Default)]
struct State {
    /// fingerprint → Unix seconds the pin was last (re)applied.
    pins: HashMap<String, u64>,
    /// fingerprint → consecutive hard-failure count (pre-pin, transient).
    failures: HashMap<String, u32>,
}

impl AdequacyLedger {
    /// Build a ledger from config, warming the in-memory pin cache from the
    /// store. A failed warm-up read is non-fatal (the cache simply starts empty).
    pub async fn load(config: &AdequacyConfig, store: AdequacyStore) -> Self {
        let mut pins = HashMap::new();
        if let Ok(rows) = store.load_all().await {
            for (fingerprint, pinned_at) in rows {
                pins.insert(fingerprint, pinned_at.max(0) as u64);
            }
        }
        Self {
            state: RwLock::new(State {
                pins,
                failures: HashMap::new(),
            }),
            store: Some(store),
            threshold: config.escalation_threshold.max(1),
            cooldown_secs: config.pin_cooldown_secs,
        }
    }

    /// An in-memory-only ledger (no persistence) — for tests.
    #[cfg(test)]
    pub fn in_memory(threshold: u32, cooldown_secs: u64) -> Self {
        Self {
            state: RwLock::new(State::default()),
            store: None,
            threshold: threshold.max(1),
            cooldown_secs,
        }
    }

    /// Whether `fingerprint` is currently pinned to the escalation tier. Sync,
    /// for the router's hot path; a pin past its cooldown reads as not pinned.
    pub fn is_pinned(&self, fingerprint: &str) -> bool {
        let guard = self.state.read().unwrap_or_else(PoisonError::into_inner);
        match guard.pins.get(fingerprint) {
            Some(&pinned_at) => {
                self.cooldown_secs == 0 || now_unix().saturating_sub(pinned_at) < self.cooldown_secs
            }
            None => false,
        }
    }

    /// Record a *downgraded* request's outcome for `fingerprint`. An `inadequate`
    /// outcome accrues toward a pin; an adequate one clears the pre-pin tally.
    /// Async because a newly created pin is persisted.
    pub async fn observe(&self, fingerprint: &str, inadequate: bool) {
        let newly_pinned = {
            let mut guard = self.state.write().unwrap_or_else(PoisonError::into_inner);
            if !inadequate {
                // A clean outcome resets the pre-pin tally; an existing pin is
                // left to decay on its own cooldown.
                guard.failures.remove(fingerprint);
                None
            } else {
                let count = guard.failures.entry(fingerprint.to_string()).or_insert(0);
                *count += 1;
                if *count >= self.threshold {
                    let pinned_at = now_unix();
                    guard.pins.insert(fingerprint.to_string(), pinned_at);
                    guard.failures.remove(fingerprint);
                    Some(pinned_at)
                } else {
                    None
                }
            }
        };
        // Persist outside the lock — never hold a std lock across `.await`.
        if let (Some(pinned_at), Some(store)) = (newly_pinned, &self.store) {
            // Best-effort: a failed write only means the pin won't survive a
            // restart, never a dropped or blocked request.
            let _ = store.upsert_pin(fingerprint, pinned_at as i64).await;
        }
    }
}

/// Current Unix time in seconds (saturating to 0 before the epoch).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pins_a_fingerprint_after_threshold_failures() {
        let ledger = AdequacyLedger::in_memory(2, 0);
        assert!(!ledger.is_pinned("after_edit"));
        ledger.observe("after_edit", true).await; // 1 failure < threshold 2
        assert!(!ledger.is_pinned("after_edit"));
        ledger.observe("after_edit", true).await; // 2 failures == threshold
        assert!(ledger.is_pinned("after_edit"), "pinned at the threshold");
    }

    #[tokio::test]
    async fn an_adequate_outcome_resets_the_pre_pin_tally() {
        let ledger = AdequacyLedger::in_memory(2, 0);
        ledger.observe("after_edit", true).await; // 1 failure
        ledger.observe("after_edit", false).await; // clean → reset
        ledger.observe("after_edit", true).await; // back to 1, not 2
        assert!(
            !ledger.is_pinned("after_edit"),
            "a clean outcome between failures must prevent the pin"
        );
    }

    #[tokio::test]
    async fn threshold_one_pins_on_the_first_failure() {
        let ledger = AdequacyLedger::in_memory(1, 0);
        ledger.observe("after_edit", true).await;
        assert!(ledger.is_pinned("after_edit"));
    }

    #[tokio::test]
    async fn a_pin_decays_after_its_cooldown() {
        // cooldown 0 never decays; a tiny cooldown with a back-dated pin reads as
        // expired. Drive the cooldown logic directly via the cache.
        let ledger = AdequacyLedger::in_memory(1, 60);
        ledger.observe("after_edit", true).await;
        assert!(ledger.is_pinned("after_edit"), "freshly pinned");
        // Back-date the pin beyond the cooldown.
        {
            let mut guard = ledger.state.write().unwrap_or_else(PoisonError::into_inner);
            let stale = now_unix().saturating_sub(120);
            guard.pins.insert("after_edit".to_string(), stale);
        }
        assert!(
            !ledger.is_pinned("after_edit"),
            "a pin past its cooldown reads as expired"
        );
    }

    #[tokio::test]
    async fn zero_cooldown_pin_never_decays() {
        let ledger = AdequacyLedger::in_memory(1, 0);
        ledger.observe("after_edit", true).await;
        {
            let mut guard = ledger.state.write().unwrap_or_else(PoisonError::into_inner);
            guard.pins.insert("after_edit".to_string(), 1); // ancient
        }
        assert!(
            ledger.is_pinned("after_edit"),
            "cooldown 0 means the pin is permanent"
        );
    }
}
