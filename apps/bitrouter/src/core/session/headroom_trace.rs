//! Failure-only test diagnostics. No identity, payload or error text is emitted.
use super::CoreError;

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
        if elapsed >= std::time::Duration::from_secs(1) {
            eprintln!(
                "headroom callback duration: operation={:?}, elapsed_ms={}",
                self.operation,
                elapsed.as_millis()
            );
        }
        if let Err(error) = result {
            eprintln!(
                "headroom scope returned: operation={:?}, code={:?}, commit={:?}",
                self.operation, error.code, error.commit_status
            );
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if !self.returned {
            eprintln!(
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
    milliseconds: [u128; 6],
    completed: u8,
    revision: u64,
    receipts: usize,
}
impl Transition {
    pub(super) fn new() -> Self {
        let started = std::time::Instant::now();
        Self {
            started,
            previous: started,
            milliseconds: [0; 6],
            completed: 0,
            revision: 0,
            receipts: 0,
        }
    }
    pub(super) fn snapshot(&mut self, revision: u64, receipts: usize) {
        self.revision = revision;
        self.receipts = receipts;
    }
    pub(super) fn mark(&mut self, stage: Stage) {
        let now = std::time::Instant::now();
        if let Some(value) = self.milliseconds.get_mut(stage as usize) {
            *value = now.duration_since(self.previous).as_millis();
            self.completed |= 1 << stage as u8;
        }
        self.previous = now;
    }
}
impl Drop for Transition {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        if elapsed >= std::time::Duration::from_secs(1) {
            let [commit_lock, prepare, preflight, propose, commit, adopt] = self.milliseconds;
            eprintln!(
                "headroom transition timing: revision={}, receipts={}, elapsed_ms={}, completed_stages={:06b}, commit_lock_ms={commit_lock}, prepare_ms={prepare}, choice_preflight_ms={preflight}, propose_ms={propose}, harness_commit_ms={commit}, adopt_ms={adopt}, pending_ms={}",
                self.revision,
                self.receipts,
                elapsed.as_millis(),
                self.completed,
                self.previous.elapsed().as_millis(),
            );
        }
    }
}
