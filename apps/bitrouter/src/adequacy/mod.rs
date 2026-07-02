//! Online adequacy ledger — the learned state behind adaptive policy-table
//! routing. It has two halves of the asymmetric routing rule:
//!
//! - **Safety (escalate-sticky).** When a *downgrade* keeps hard-failing, the
//!   fingerprint is **pinned** and the router escalates it to a more capable
//!   tier. A pin decays after a cooldown so a transient failure is retried.
//! - **Aggressive (downgrade discovery).** When exploration is enabled, a
//!   fingerprint the static table routes to the escalation tier (one the
//!   operator did *not* downgrade) is periodically **trialed** on the cheap
//!   explore tier; after enough adequate trials it is **locked** to the cheap
//!   tier — a downgrade discovered automatically. A trial that fails escalates
//!   and stops, exactly like the safety path.
//!
//! Together: never net-lose (a failing downgrade self-corrects) while still
//! pursuing cheaper routes (safe downgrades are found, not only hand-configured).
//! No LLM, no randomness — the trial cadence is a deterministic counter.
//!
//! The read paths ([`AdequacyLedger::is_pinned`] / [`is_locked`](AdequacyLedger::is_locked)
//! / [`should_trial`](AdequacyLedger::should_trial)) are synchronous and
//! lock-cheap because they run inside the ingress [`bitrouter_sdk::PromptTransform`];
//! the write path ([`AdequacyLedger::observe`]) is async because a new pin is
//! persisted, and runs from the [`observer::AdequacyObserveHook`] after each
//! request. Pins persist across restarts (the safety state); locks are in-memory
//! and re-discovered (an optimization, cheap to re-learn).

#[cfg(test)]
mod eval;
pub mod observer;
pub mod store;

use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use bitrouter_sdk::config::AdequacyConfig;

use self::store::AdequacyStore;

/// What the observer determined about one request, for the ledger to fold in.
pub enum Outcome {
    /// A static (operator-configured) downgrade — the served tier is the cheap
    /// tier the static table assigned this fingerprint. Consecutive failures pin.
    StaticDowngrade {
        /// Whether the request hard-failed.
        inadequate: bool,
    },
    /// An exploration candidate — the static table routes this fingerprint to the
    /// escalation tier (the operator did not downgrade it). `trialed` is whether
    /// this request was routed to the cheap explore tier (a trial or a locked
    /// route), vs left on the escalation tier (which only advances the cadence).
    Exploration {
        /// Whether this request went to the explore tier.
        trialed: bool,
        /// Whether the request hard-failed (only meaningful when `trialed`).
        inadequate: bool,
    },
}

/// Learned escalation + exploration state, keyed by request fingerprint.
pub struct AdequacyLedger {
    state: RwLock<State>,
    /// Persistence for pins. `None` in unit tests / when no db is wired.
    store: Option<AdequacyStore>,
    /// Consecutive hard failures on a downgrade before it is pinned (>= 1).
    threshold: u32,
    /// Seconds a pin lasts before the downgrade is re-attempted. `0` = no decay.
    cooldown_secs: u64,
    /// Trial cadence: ~one in `explore_interval` candidate requests is a trial.
    explore_interval: u32,
    /// Consecutive adequate trials before a fingerprint locks to the cheap tier.
    explore_threshold: u32,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
}

/// Per-fingerprint learned state.
#[derive(Default)]
struct Entry {
    /// Consecutive hard-failure tally toward a pin (pre-pin, transient).
    failures: u32,
    /// Unix seconds the escalation pin was applied; `None` = not pinned.
    pinned_at: Option<u64>,
    /// Exploration candidate requests observed (drives the trial cadence).
    observed: u32,
    /// Consecutive adequate trials toward a lock.
    adequate_trials: u32,
    /// Locked to the explore tier — a discovered, learned downgrade.
    locked: bool,
}

impl AdequacyLedger {
    /// Build a ledger from config, warming the in-memory pin cache from the
    /// store. A failed warm-up read is non-fatal (the cache starts empty).
    pub async fn load(config: &AdequacyConfig, store: AdequacyStore) -> Self {
        let mut entries: HashMap<String, Entry> = HashMap::new();
        if let Ok(rows) = store.load_all().await {
            for (fingerprint, pinned_at) in rows {
                entries.entry(fingerprint).or_default().pinned_at = Some(pinned_at.max(0) as u64);
            }
        }
        Self {
            state: RwLock::new(State { entries }),
            store: Some(store),
            threshold: config.escalation_threshold.max(1),
            cooldown_secs: config.pin_cooldown_secs,
            explore_interval: config.explore_interval.max(1),
            explore_threshold: config.explore_threshold.max(1),
        }
    }

    /// An in-memory-only ledger (no persistence) with the given pin threshold /
    /// cooldown and trial-every-request, lock-on-first-adequate-trial — for tests
    /// that only exercise the safety path.
    #[cfg(test)]
    pub fn in_memory(threshold: u32, cooldown_secs: u64) -> Self {
        Self::in_memory_explore(threshold, cooldown_secs, 1, 1)
    }

    /// An in-memory-only ledger with full exploration parameters — for tests.
    #[cfg(test)]
    pub fn in_memory_explore(
        threshold: u32,
        cooldown_secs: u64,
        explore_interval: u32,
        explore_threshold: u32,
    ) -> Self {
        Self {
            state: RwLock::new(State::default()),
            store: None,
            threshold: threshold.max(1),
            cooldown_secs,
            explore_interval: explore_interval.max(1),
            explore_threshold: explore_threshold.max(1),
        }
    }

    /// Whether `fingerprint` is currently pinned to the escalation tier. Sync,
    /// for the router's hot path; a pin past its cooldown reads as not pinned.
    pub fn is_pinned(&self, fingerprint: &str) -> bool {
        let guard = self.state.read().unwrap_or_else(PoisonError::into_inner);
        guard
            .entries
            .get(fingerprint)
            .and_then(|e| e.pinned_at)
            .is_some_and(|pinned_at| {
                self.cooldown_secs == 0 || now_unix().saturating_sub(pinned_at) < self.cooldown_secs
            })
    }

    /// Whether `fingerprint` is locked to the explore tier (a learned downgrade).
    pub fn is_locked(&self, fingerprint: &str) -> bool {
        let guard = self.state.read().unwrap_or_else(PoisonError::into_inner);
        guard.entries.get(fingerprint).is_some_and(|e| e.locked)
    }

    /// Whether the next request for an exploration candidate `fingerprint` should
    /// be a trial on the explore tier (deterministic cadence). The router only
    /// consults this when the fingerprint is neither pinned nor locked.
    pub fn should_trial(&self, fingerprint: &str) -> bool {
        let guard = self.state.read().unwrap_or_else(PoisonError::into_inner);
        match guard.entries.get(fingerprint) {
            Some(e) if !e.locked => e.observed > 0 && e.observed % self.explore_interval == 0,
            // Unseen candidate: observed == 0, not yet a trial (the first request
            // routes to the safe escalation tier and advances the cadence).
            _ => false,
        }
    }

    /// Fold a request's [`Outcome`] into the ledger. Async because a newly
    /// created pin is persisted.
    pub async fn observe(&self, fingerprint: &str, outcome: Outcome) {
        let newly_pinned = {
            let mut guard = self.state.write().unwrap_or_else(PoisonError::into_inner);
            let entry = guard.entries.entry(fingerprint.to_string()).or_default();
            match outcome {
                Outcome::StaticDowngrade { inadequate } => {
                    if inadequate {
                        entry.failures += 1;
                        (entry.failures >= self.threshold).then(|| pin(entry))
                    } else {
                        // A clean outcome resets the pre-pin tally; an existing
                        // pin decays on its own cooldown.
                        entry.failures = 0;
                        None
                    }
                }
                Outcome::Exploration {
                    trialed,
                    inadequate,
                } => {
                    // Every candidate request (trialed or not) advances the cadence.
                    entry.observed = entry.observed.saturating_add(1);
                    if !trialed {
                        None
                    } else if inadequate {
                        // A failed trial / locked route: un-learn the downgrade
                        // and escalate (sticky), like the safety path.
                        entry.adequate_trials = 0;
                        entry.locked = false;
                        Some(pin(entry))
                    } else if !entry.locked {
                        entry.adequate_trials += 1;
                        if entry.adequate_trials >= self.explore_threshold {
                            entry.locked = true;
                        }
                        None
                    } else {
                        None
                    }
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

/// Apply an escalation pin to `entry`, clearing the pre-pin tally, and return the
/// pin time (Unix seconds) so the caller can persist it.
fn pin(entry: &mut Entry) -> u64 {
    let pinned_at = now_unix();
    entry.pinned_at = Some(pinned_at);
    entry.failures = 0;
    pinned_at
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

    fn fail() -> Outcome {
        Outcome::StaticDowngrade { inadequate: true }
    }
    fn ok() -> Outcome {
        Outcome::StaticDowngrade { inadequate: false }
    }
    fn trial(inadequate: bool) -> Outcome {
        Outcome::Exploration {
            trialed: true,
            inadequate,
        }
    }
    fn non_trial() -> Outcome {
        Outcome::Exploration {
            trialed: false,
            inadequate: false,
        }
    }

    // ---- safety half (pins) ----

    #[tokio::test]
    async fn pins_a_fingerprint_after_threshold_failures() {
        let ledger = AdequacyLedger::in_memory(2, 0);
        ledger.observe("after_edit", fail()).await;
        assert!(!ledger.is_pinned("after_edit"));
        ledger.observe("after_edit", fail()).await;
        assert!(ledger.is_pinned("after_edit"));
    }

    #[tokio::test]
    async fn an_adequate_outcome_resets_the_pre_pin_tally() {
        let ledger = AdequacyLedger::in_memory(2, 0);
        ledger.observe("after_edit", fail()).await;
        ledger.observe("after_edit", ok()).await;
        ledger.observe("after_edit", fail()).await;
        assert!(!ledger.is_pinned("after_edit"));
    }

    #[tokio::test]
    async fn a_pin_decays_after_its_cooldown() {
        let ledger = AdequacyLedger::in_memory(1, 60);
        ledger.observe("after_edit", fail()).await;
        assert!(ledger.is_pinned("after_edit"));
        {
            let mut guard = ledger.state.write().unwrap_or_else(PoisonError::into_inner);
            let stale = now_unix().saturating_sub(120);
            guard.entries.get_mut("after_edit").unwrap().pinned_at = Some(stale);
        }
        assert!(!ledger.is_pinned("after_edit"));
    }

    #[tokio::test]
    async fn zero_cooldown_pin_never_decays() {
        let ledger = AdequacyLedger::in_memory(1, 0);
        ledger.observe("after_edit", fail()).await;
        {
            let mut guard = ledger.state.write().unwrap_or_else(PoisonError::into_inner);
            guard.entries.get_mut("after_edit").unwrap().pinned_at = Some(1);
        }
        assert!(ledger.is_pinned("after_edit"));
    }

    // ---- aggressive half (exploration) ----

    #[tokio::test]
    async fn trial_cadence_fires_every_interval() {
        // interval 3: the first two candidate observations don't trial; the third
        // does (observed cycles 1,2,3 → trial at 3).
        let ledger = AdequacyLedger::in_memory_explore(1, 0, 3, 2);
        assert!(!ledger.should_trial("opening")); // unseen
        ledger.observe("opening", non_trial()).await; // observed=1
        assert!(!ledger.should_trial("opening"));
        ledger.observe("opening", non_trial()).await; // observed=2
        assert!(!ledger.should_trial("opening"));
        ledger.observe("opening", non_trial()).await; // observed=3
        assert!(ledger.should_trial("opening"), "trial due at the interval");
    }

    #[tokio::test]
    async fn locks_after_enough_adequate_trials() {
        let ledger = AdequacyLedger::in_memory_explore(1, 0, 1, 2);
        ledger.observe("opening", trial(false)).await; // 1 adequate trial
        assert!(!ledger.is_locked("opening"));
        ledger.observe("opening", trial(false)).await; // 2 → lock
        assert!(ledger.is_locked("opening"), "locked after the threshold");
    }

    #[tokio::test]
    async fn a_failed_trial_escalates_and_does_not_lock() {
        let ledger = AdequacyLedger::in_memory_explore(1, 0, 1, 2);
        ledger.observe("opening", trial(false)).await; // 1 adequate
        ledger.observe("opening", trial(true)).await; // failure → pin, reset
        assert!(!ledger.is_locked("opening"));
        assert!(ledger.is_pinned("opening"), "a failed trial escalates");
    }

    #[tokio::test]
    async fn a_failed_locked_route_unlocks_and_escalates() {
        let ledger = AdequacyLedger::in_memory_explore(1, 0, 1, 1);
        ledger.observe("opening", trial(false)).await; // locks (threshold 1)
        assert!(ledger.is_locked("opening"));
        ledger.observe("opening", trial(true)).await; // locked route fails
        assert!(
            !ledger.is_locked("opening"),
            "a failed locked route un-locks"
        );
        assert!(ledger.is_pinned("opening"));
    }
}
