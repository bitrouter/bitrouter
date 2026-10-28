//! Test-only wall-time observations; no identity, payload or error text.
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(super) enum Operation {
    NativeCommit,
    NativePersist,
}

pub(super) enum CommitStage {
    StateLock,
    AppendValidation,
    ChoiceApply,
    Assemble,
    Persist,
    AckTail,
}

pub(super) enum PersistStage {
    Recovery,
    Encode,
    StoreSave,
    Adopt,
}

#[derive(Clone, Copy, Debug)]
enum Outcome {
    Unfinished,
    Success,
    Replay,
}

pub(super) struct Scope {
    operation: Operation,
    started: Instant,
    previous: Instant,
    milliseconds: [u128; 6],
    completed: u8,
    state_revision: Option<u64>,
    store_revision: Option<u64>,
    outcome: Outcome,
}

impl Scope {
    pub(super) fn new(operation: Operation) -> Self {
        let started = Instant::now();
        Self {
            operation,
            started,
            previous: started,
            milliseconds: [0; 6],
            completed: 0,
            state_revision: None,
            store_revision: None,
            outcome: Outcome::Unfinished,
        }
    }

    pub(super) fn revisions(&mut self, state: u64, store: u64) {
        self.state_revision = Some(state);
        self.store_revision = Some(store);
    }

    pub(super) fn mark_commit(&mut self, stage: CommitStage) {
        self.mark(stage as usize);
    }

    pub(super) fn mark_persist(&mut self, stage: PersistStage) {
        self.mark(stage as usize);
    }

    fn mark(&mut self, index: usize) {
        let now = Instant::now();
        if let Some(value) = self.milliseconds.get_mut(index) {
            *value = now.duration_since(self.previous).as_millis();
            self.completed |= 1 << index;
        }
        self.previous = now;
    }

    pub(super) fn succeeded(&mut self) {
        self.outcome = Outcome::Success;
    }

    pub(super) fn replayed(&mut self) {
        self.outcome = Outcome::Replay;
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        // Separate selected populations: a subsecond persist can be observed
        // independently of its enclosing commit. These are not deadline changes.
        let threshold = match self.operation {
            Operation::NativeCommit => Duration::from_millis(500),
            Operation::NativePersist => Duration::from_millis(250),
        };
        if elapsed < threshold && !matches!(self.outcome, Outcome::Unfinished) {
            return;
        }
        let pending = self.previous.elapsed().as_millis();
        let unwinding = std::thread::panicking();
        match self.operation {
            Operation::NativeCommit => {
                let [lock, validate, choices, assemble, persist, tail] = self.milliseconds;
                eprintln!(
                    "headroom native commit timing: state_revision={:?}, store_revision={:?}, elapsed_ms={}, completed_stages={:06b}, state_lock_ms={lock}, append_validation_ms={validate}, choice_apply_ms={choices}, assemble_ms={assemble}, persist_ms={persist}, ack_tail_ms={tail}, pending_ms={pending}, outcome={:?}, unwinding={unwinding}",
                    self.state_revision,
                    self.store_revision,
                    elapsed.as_millis(),
                    self.completed,
                    self.outcome,
                );
            }
            Operation::NativePersist => {
                let [recovery, encode, save, adopt, _, _] = self.milliseconds;
                eprintln!(
                    "headroom native persist timing: state_revision={:?}, store_revision={:?}, elapsed_ms={}, completed_stages={:04b}, recovery_ms={recovery}, encode_ms={encode}, store_save_ms={save}, adopt_ms={adopt}, pending_ms={pending}, outcome={:?}, unwinding={unwinding}",
                    self.state_revision,
                    self.store_revision,
                    elapsed.as_millis(),
                    self.completed,
                    self.outcome,
                );
            }
        }
    }
}
