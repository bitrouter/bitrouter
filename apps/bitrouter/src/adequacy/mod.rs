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
//! request. Pins and positive exploration state persist across restarts, so
//! policy rounds keep trial cadence and learned cheap-route locks.

#[cfg(test)]
mod eval;
pub mod observer;
pub mod settlement;
pub mod store;

use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use bitrouter_sdk::config::AdequacyConfig;

use self::store::AdequacyStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InadequacyCause {
    None,
    ProviderTransient,
    ProviderPermanent,
    Protocol,
    Auth,
    Client,
    Semantic,
}

impl InadequacyCause {
    fn is_none(self) -> bool {
        matches!(self, Self::None)
    }

    fn is_ignored(self) -> bool {
        matches!(self, Self::Auth | Self::Client)
    }

    fn is_transient(self) -> bool {
        matches!(self, Self::ProviderTransient)
    }

    fn pins_immediately(self) -> bool {
        matches!(self, Self::Semantic)
    }

    fn counts_as_non_transient_failure(self) -> bool {
        matches!(self, Self::ProviderPermanent | Self::Protocol)
    }
}

/// What the observer determined about one request, for the ledger to fold in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A static (operator-configured) downgrade — the served tier is the cheap
    /// tier the static table assigned this fingerprint. Consecutive failures pin.
    StaticDowngrade {
        /// Why the request was inadequate, or `None` when it completed cleanly.
        cause: InadequacyCause,
    },
    /// An exploration candidate — the static table routes this fingerprint to the
    /// escalation tier (the operator did not downgrade it). `trialed` is whether
    /// this request was routed to the cheap explore tier (a trial or a locked
    /// route), vs left on the escalation tier (which only advances the cadence).
    Exploration {
        /// Whether this request went to the explore tier.
        trialed: bool,
        /// Why the request was inadequate, or `None` when it completed cleanly.
        cause: InadequacyCause,
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
    /// Distinct task-level successes required before a request lock is effective.
    min_semantic_successes_for_lock: u32,
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
    /// Consecutive transient provider-failure tally toward a reliability pin.
    transient_failures: u32,
    /// Unix seconds the escalation pin was applied; `None` = not pinned.
    pinned_at: Option<u64>,
    /// Exploration candidate requests observed (drives the trial cadence).
    observed: u32,
    /// Consecutive adequate trials toward a lock.
    adequate_trials: u32,
    /// Locked to the explore tier — a discovered, learned downgrade.
    locked: bool,
    /// Distinct successful benchmark tasks attributed to this cheap transition.
    semantic_successes: u32,
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
        if let Ok(rows) = store.load_exploration_all().await {
            for row in rows {
                let entry = entries.entry(row.fingerprint).or_default();
                entry.observed = row.observed;
                entry.adequate_trials = row.adequate_trials;
                entry.locked = row.locked;
            }
        }
        if let Ok(counts) = store.load_semantic_success_counts().await {
            for (fingerprint, semantic_successes) in counts {
                entries.entry(fingerprint).or_default().semantic_successes = semantic_successes;
            }
        }
        Self {
            state: RwLock::new(State { entries }),
            store: Some(store),
            threshold: config.escalation_threshold.max(1),
            cooldown_secs: config.pin_cooldown_secs,
            explore_interval: config.explore_interval.max(1),
            explore_threshold: config.explore_threshold.max(1),
            min_semantic_successes_for_lock: config.min_semantic_successes_for_lock,
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
            min_semantic_successes_for_lock: 0,
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
        self.is_locked_with_semantic_threshold(fingerprint, 0)
    }

    pub fn is_locked_with_semantic_threshold(
        &self,
        fingerprint: &str,
        minimum_semantic_successes: u32,
    ) -> bool {
        let guard = self.state.read().unwrap_or_else(PoisonError::into_inner);
        let threshold = self.semantic_success_threshold(minimum_semantic_successes);
        guard
            .entries
            .get(fingerprint)
            .is_some_and(|e| e.locked && e.semantic_successes >= threshold)
    }

    /// Whether request-level completion evidence has qualified the cheap route,
    /// independent of the task-level semantic-success gate.
    pub fn is_request_qualified(&self, fingerprint: &str) -> bool {
        let guard = self.state.read().unwrap_or_else(PoisonError::into_inner);
        guard.entries.get(fingerprint).is_some_and(|e| e.locked)
    }

    pub fn semantic_successes(&self, fingerprint: &str) -> u32 {
        let guard = self.state.read().unwrap_or_else(PoisonError::into_inner);
        guard
            .entries
            .get(fingerprint)
            .map_or(0, |entry| entry.semantic_successes)
    }

    pub fn semantic_success_threshold(&self, minimum_semantic_successes: u32) -> u32 {
        self.min_semantic_successes_for_lock
            .max(minimum_semantic_successes)
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
        let mut exploration_snapshot: Option<(String, u32, u32, bool)> = None;
        let newly_pinned = {
            let mut guard = self.state.write().unwrap_or_else(PoisonError::into_inner);
            let entry = guard.entries.entry(fingerprint.to_string()).or_default();
            match outcome {
                Outcome::StaticDowngrade { cause } => self.apply_failure_cause(entry, cause, false),
                Outcome::Exploration { trialed, cause } => {
                    // Every candidate request (trialed or not) advances the cadence.
                    entry.observed = entry.observed.saturating_add(1);
                    let newly_pinned = if !trialed {
                        None
                    } else if cause.is_none() {
                        entry.failures = 0;
                        entry.transient_failures = 0;
                        entry.adequate_trials += 1;
                        if !entry.locked && entry.adequate_trials >= self.explore_threshold {
                            entry.locked = true;
                        }
                        None
                    } else {
                        entry.adequate_trials = 0;
                        self.apply_failure_cause(entry, cause, true)
                    };
                    exploration_snapshot = Some((
                        fingerprint.to_string(),
                        entry.observed,
                        entry.adequate_trials,
                        entry.locked,
                    ));
                    newly_pinned
                }
            }
        };
        // Persist outside the lock — never hold a std lock across `.await`.
        if let (Some(pinned_at), Some(store)) = (newly_pinned, &self.store) {
            // Best-effort: a failed write only means the pin won't survive a
            // restart, never a dropped or blocked request.
            if let Err(error) = store.upsert_pin(fingerprint, pinned_at as i64).await {
                tracing::warn!(
                    %error,
                    fingerprint,
                    "adequacy pin persistence failed"
                );
            }
        }
        if let (Some((fingerprint, observed, adequate_trials, locked)), Some(store)) =
            (exploration_snapshot, &self.store)
            && let Err(error) = store
                .upsert_exploration(&fingerprint, observed, adequate_trials, locked)
                .await
        {
            tracing::warn!(
                %error,
                fingerprint,
                observed,
                adequate_trials,
                locked,
                "adequacy exploration persistence failed"
            );
        }
    }

    fn apply_failure_cause(
        &self,
        entry: &mut Entry,
        cause: InadequacyCause,
        exploration_failure: bool,
    ) -> Option<u64> {
        if cause.is_none() {
            entry.failures = 0;
            entry.transient_failures = 0;
            return None;
        }
        if cause.is_ignored() {
            return None;
        }
        if cause.pins_immediately() {
            if exploration_failure {
                entry.locked = false;
            }
            return Some(pin(entry));
        }
        if cause.is_transient() {
            entry.transient_failures = entry.transient_failures.saturating_add(1);
            entry.failures = 0;
            if entry.transient_failures >= self.transient_threshold() {
                if exploration_failure {
                    entry.locked = false;
                }
                return Some(pin(entry));
            }
            return None;
        }
        if cause.counts_as_non_transient_failure() {
            entry.failures = entry.failures.saturating_add(1);
            entry.transient_failures = 0;
            if entry.failures >= self.threshold {
                if exploration_failure {
                    entry.locked = false;
                }
                return Some(pin(entry));
            }
        }
        None
    }

    fn transient_threshold(&self) -> u32 {
        self.threshold.max(2)
    }
}

/// Apply an escalation pin to `entry`, clearing the pre-pin tally, and return the
/// pin time (Unix seconds) so the caller can persist it.
fn pin(entry: &mut Entry) -> u64 {
    let pinned_at = now_unix();
    entry.pinned_at = Some(pinned_at);
    entry.failures = 0;
    entry.transient_failures = 0;
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
        Outcome::StaticDowngrade {
            cause: InadequacyCause::ProviderPermanent,
        }
    }
    fn ok() -> Outcome {
        Outcome::StaticDowngrade {
            cause: InadequacyCause::None,
        }
    }
    fn trial(inadequate: bool) -> Outcome {
        Outcome::Exploration {
            trialed: true,
            cause: if inadequate {
                InadequacyCause::ProviderPermanent
            } else {
                InadequacyCause::None
            },
        }
    }
    fn trial_with(cause: InadequacyCause) -> Outcome {
        Outcome::Exploration {
            trialed: true,
            cause,
        }
    }
    fn non_trial() -> Outcome {
        Outcome::Exploration {
            trialed: false,
            cause: InadequacyCause::None,
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
    async fn transient_provider_error_does_not_permanently_pin_on_first_failure() {
        let ledger = AdequacyLedger::in_memory_explore(1, 300, 1, 3);
        ledger
            .observe(
                "tool_followup",
                trial_with(InadequacyCause::ProviderTransient),
            )
            .await;

        assert!(!ledger.is_pinned("tool_followup"));
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
