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
