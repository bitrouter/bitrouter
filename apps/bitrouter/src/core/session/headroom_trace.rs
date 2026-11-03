//! Test failure diagnostics and opt-in timings; no identity, payload or error text.
use super::CoreError;
use std::io::Write;

fn timing_enabled() -> bool {
    std::env::var("BITROUTER_TEST_CHECKPOINT_TIMING").is_ok_and(|value| value == "1")
}

#[derive(Debug)]
enum Operation {
    RoutingCallback,
    ContextCatalogue,
    ChoiceIntent,
    ChoiceOutcome,
    ModelPreparing,
    ModelPlan,
    ContextApplied,
    PreparationIntent,
    PreparationOutcome,
    CountIntent,
    CountOutcome,
    ValidationIntent,
    ValidationOutcome,
    ModelIntent,
    ModelOutcome,
    ProviderIntent,
    ProviderOutcome,
    Other,
}

pub(super) struct Scope {
    operation: Operation,
    returned: bool,
    started: std::time::Instant,
}

impl Scope {
    pub(super) fn new(kind: &str) -> Self {
        let operation = match kind {
            "prepare_routing" => Operation::RoutingCallback,
            "context.catalogued" => Operation::ContextCatalogue,
            "context.decision.intent" => Operation::ChoiceIntent,
            "context.decision.outcome" => Operation::ChoiceOutcome,
            "model.step.preparing" => Operation::ModelPreparing,
            "model.plan" => Operation::ModelPlan,
            "context.view.applied" => Operation::ContextApplied,
            "preparation.work.intent" => Operation::PreparationIntent,
            "preparation.work.outcome" => Operation::PreparationOutcome,
            "model.input_count.intent" => Operation::CountIntent,
            "model.input_count.outcome" => Operation::CountOutcome,
            "context.validation.intent" => Operation::ValidationIntent,
            "context.validation.outcome" => Operation::ValidationOutcome,
            "model.attempt.intent" => Operation::ModelIntent,
            "model.attempt.outcome" => Operation::ModelOutcome,
            "provider.work.intent" => Operation::ProviderIntent,
            "provider.work.outcome" => Operation::ProviderOutcome,
            _ => Operation::Other,
        };
        Self {
            operation,
            returned: false,
            started: std::time::Instant::now(),
        }
    }

    pub(super) fn returned<T>(&mut self, result: &Result<T, CoreError>) {
        self.returned = true;
        let elapsed = self.started.elapsed();
        if elapsed >= std::time::Duration::from_secs(1) && timing_enabled() {
            let _ = writeln!(
                std::io::stderr().lock(),
                "headroom callback duration: operation={:?}, elapsed_ms={}",
                self.operation,
                elapsed.as_millis()
            );
        }
        if let Err(error) = result {
            let _ = writeln!(
                std::io::stderr().lock(),
                "headroom scope returned: operation={:?}, code={:?}, commit={:?}",
                self.operation,
                error.code,
                error.commit_status
            );
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if !self.returned {
            let _ = writeln!(
                std::io::stderr().lock(),
                "headroom scope dropped: operation={:?}, unwinding={}",
                self.operation,
                std::thread::panicking()
            );
        }
    }
}

// Six fixed transition stages. Timing is test-only and emits no checkpoint data.
#[derive(Clone, Copy)]
pub(super) enum Stage {
    CommitLock,
    Prepare,
    ChoicePreflight,
    Propose,
    HarnessCommit,
    Adopt,
}

pub(super) struct Transition {
    started: std::time::Instant,
    previous: std::time::Instant,
    microseconds: [u128; 6],
    completed: u8,
    enabled: bool,
}
impl Transition {
    pub(super) fn new() -> Self {
        let started = std::time::Instant::now();
        Self {
            started,
            previous: started,
            microseconds: [0; 6],
            completed: 0,
            enabled: timing_enabled(),
        }
    }
    pub(super) fn mark(&mut self, stage: Stage) {
        let now = std::time::Instant::now();
        if let Some(value) = self.microseconds.get_mut(stage as usize) {
            *value = now.duration_since(self.previous).as_micros();
            self.completed |= 1 << stage as u8;
        }
        self.previous = now;
    }
}
impl Drop for Transition {
    fn drop(&mut self) {
        if self.enabled {
            let now = std::time::Instant::now();
            record(Record::Transition {
                microseconds: self.microseconds,
                completed: self.completed,
                elapsed: now.duration_since(self.started).as_micros(),
                pending: now.duration_since(self.previous).as_micros(),
                unwinding: std::thread::panicking(),
            });
        }
    }
}

// Fixed regions, not a general tracing API. Capacity is inside Prepare;
// JournalApply is inside HarnessCommit; CoreRestore may contain transitions.
// Their inclusive wall times must not be added to the six stage totals.
// ThreadTransaction covers every Thread submission, including non-Port writes
// and owner work that can outlive a cancelled Port waiter. Its process totals
// cannot be subtracted from PortPersist totals.
#[derive(Clone, Copy)]
pub(crate) enum RegionKind {
    Capacity,
    #[cfg(feature = "harness")]
    JournalApply,
    #[cfg(feature = "harness")]
    CoreRestore,
    #[cfg(feature = "harness")]
    PortPrepare,
    #[cfg(feature = "harness")]
    PortProjection,
    #[cfg(feature = "harness")]
    PortPersist,
    #[cfg(feature = "harness")]
    ThreadTransaction,
}

pub(crate) struct Region {
    kind: RegionKind,
    started: Option<std::time::Instant>,
}
impl Region {
    pub(crate) fn new(kind: RegionKind) -> Self {
        Self {
            kind,
            started: timing_enabled().then(std::time::Instant::now),
        }
    }

    pub(crate) fn returned(&mut self, ok: bool) {
        if let Some(started) = self.started.take() {
            record(Record::Region {
                kind: self.kind,
                outcome: if ok { 0 } else { 1 },
                elapsed: started.elapsed().as_micros(),
            });
        }
    }
}
impl Drop for Region {
    fn drop(&mut self) {
        if let Some(started) = self.started.take() {
            // Cancellation/unwinding/early return is not an observed Err or Ok.
            record(Record::Region {
                kind: self.kind,
                outcome: 2,
                elapsed: started.elapsed().as_micros(),
            });
        }
    }
}

#[derive(Clone, Copy)]
struct Stats {
    count: u64,
    sum_us: u128,
    max_us: u128,
}
impl Stats {
    const ZERO: Self = Self {
        count: 0,
        sum_us: 0,
        max_us: 0,
    };

    fn add(&mut self, micros: u128) -> bool {
        let (Some(count), Some(sum)) = (self.count.checked_add(1), self.sum_us.checked_add(micros))
        else {
            return false;
        };
        self.count = count;
        self.sum_us = sum;
        self.max_us = self.max_us.max(micros);
        true
    }

    fn tuple(self) -> (u64, u128, u128) {
        (self.count, self.sum_us, self.max_us)
    }
}

enum Record {
    Transition {
        microseconds: [u128; 6],
        completed: u8,
        elapsed: u128,
        pending: u128,
        unwinding: bool,
    },
    Region {
        kind: RegionKind,
        outcome: usize,
        elapsed: u128,
    },
}

#[derive(Clone, Copy, Debug)]
enum Invalid {
    Poisoned,
    OverflowOrIndex,
}

#[derive(Clone, Copy)]
struct Tracker {
    transitions: Stats,
    stages: [Stats; 6],
    masks: [u64; 64],
    partial_pending: Stats,
    unwinding: u64,
    // Region order: Capacity, JournalApply, CoreRestore, PortPrepare,
    // PortProjection, PortPersist, ThreadTransaction.
    // Outcome order: returned Ok, returned Err, no observed return.
    regions: [[Stats; 3]; 7],
    invalid: Option<Invalid>,
}
impl Tracker {
    const EMPTY: Self = Self {
        transitions: Stats::ZERO,
        stages: [Stats::ZERO; 6],
        masks: [0; 64],
        partial_pending: Stats::ZERO,
        unwinding: 0,
        regions: [[Stats::ZERO; 3]; 7],
        invalid: None,
    };

    fn observe(&mut self, event: Record) -> bool {
        match event {
            Record::Transition {
                microseconds,
                completed,
                elapsed,
                pending,
                unwinding,
            } => {
                let mut valid = self.transitions.add(elapsed);
                for (index, (stats, micros)) in self.stages.iter_mut().zip(microseconds).enumerate()
                {
                    if completed & (1 << index) != 0 {
                        valid &= stats.add(micros);
                    }
                }
                let Some(mask) = self.masks.get_mut(usize::from(completed)) else {
                    return false;
                };
                let Some(count) = mask.checked_add(1) else {
                    return false;
                };
                *mask = count;
                if completed != 0b11_1111 {
                    valid &= self.partial_pending.add(pending);
                }
                let Some(count) = self.unwinding.checked_add(u64::from(unwinding)) else {
                    return false;
                };
                self.unwinding = count;
                valid
            }
            Record::Region {
                kind,
                outcome,
                elapsed,
            } => self
                .regions
                .get_mut(kind as usize)
                .and_then(|outcomes| outcomes.get_mut(outcome))
                .is_some_and(|stats| stats.add(elapsed)),
        }
    }
}

// One process-local, fixed-size tracker. It owns no application object. The
// mutex is released before output; no guard crosses an await. Poison/overflow
// invalidates diagnostics once, without altering any original operation result.
static TRACKER: std::sync::Mutex<Tracker> = std::sync::Mutex::new(Tracker::EMPTY);

fn record(event: Record) {
    let transition = matches!(&event, Record::Transition { .. });
    let summary = {
        let (mut tracker, poisoned) = match TRACKER.lock() {
            Ok(tracker) => (tracker, false),
            Err(error) => (error.into_inner(), true),
        };
        // Preserve a first early failure/cancellation even before transition
        // 256. At most one partial and fourteen region-outcome snapshots are added.
        let first_interruption = match &event {
            Record::Transition { completed, .. } => {
                *completed != 0b11_1111 && tracker.partial_pending.count == 0
            }
            Record::Region { kind, outcome, .. } => {
                *outcome != 0
                    && tracker
                        .regions
                        .get(*kind as usize)
                        .and_then(|outcomes| outcomes.get(*outcome))
                        .is_some_and(|stats| stats.count == 0)
            }
        };
        let was_valid = tracker.invalid.is_none();
        if poisoned && was_valid {
            tracker.invalid = Some(Invalid::Poisoned);
        }
        if tracker.invalid.is_none() && !tracker.observe(event) {
            tracker.invalid = Some(Invalid::OverflowOrIndex);
        }
        if (was_valid && tracker.invalid.is_some())
            || (tracker.invalid.is_none()
                && (first_interruption
                    || (transition && tracker.transitions.count.is_multiple_of(256))))
        {
            Some(*tracker)
        } else {
            None
        }
    };
    if let Some(summary) = summary {
        // A mask marks reached boundaries, not a successful transition. Active
        // intervals and the unprinted tail are absent if the process is killed.
        let _ = writeln!(
            std::io::stderr().lock(),
            "native checkpoint phases: diagnostics_valid={} invalid={:?} tuple=count,sum_us,max_us transition_drops={:?} stages_commit_lock_prepare_choice_preflight_propose_harness_commit_adopt={:?} masks_0_to_63={:?} partial_pending={:?} unwinding={} region_outcomes=ok,err,unfinished regions_capacity_journal_apply_core_restore_port_prepare_port_projection_port_persist_thread_transaction={:?} inclusive_wall_time=true capacity_in=prepare port_regions_in=harness_commit thread_transaction_scope=all_commit_thread_serialized thread_transaction_relation=overlap_only restore_contains=transitions not_a_test_verdict=true",
            summary.invalid.is_none(),
            summary.invalid,
            summary.transitions.tuple(),
            summary.stages.map(Stats::tuple),
            summary.masks,
            summary.partial_pending.tuple(),
            summary.unwinding,
            summary.regions.map(|outcomes| outcomes.map(Stats::tuple)),
        );
    }
}
