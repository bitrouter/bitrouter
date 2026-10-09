//! Native run limits use the Core ledger, including retired child work.

use crate::agent::{AgentConfig, RunReport};
use crate::core::accounting::work::{CostWorkKind, CostWorkState};
use crate::core::protocol::{CoreError, ErrorCode};
use crate::core::session::SessionSnapshot;

#[derive(Clone)]
pub(super) struct Budget {
    config: AgentConfig,
    prior_spend: u64,
    prior_steps: u32,
    prior_tools: u32,
    prior_duration: u64,
    pub(super) spent: u64,
    pub(super) tool_calls: u32,
    pub(super) reason: Option<String>,
}

impl Budget {
    pub(super) fn new(config: &AgentConfig, report: &RunReport) -> Self {
        Self {
            config: config.clone(),
            prior_spend: report.estimated_spend_microusd,
            prior_steps: report.steps,
            prior_tools: report.tool_calls,
            prior_duration: report.active_duration_ms,
            spent: report.estimated_spend_microusd,
            tool_calls: 0,
            reason: None,
        }
    }

    pub(super) fn observe(&mut self, state: &SessionSnapshot) {
        let Some(run) = &state.run else { return };
        if let Some(turn) = state.root_turn()
            && turn.status == crate::core::session::AgentStatus::Failed
            && let Some(step) = turn.steps.last()
            && step.plan.is_none()
            && let Some(limit) = turn.input.context_limit_bytes
            && step.context.prompt_bytes > limit
            && !state.context_store.executions.contains_key(&step.step_id)
        {
            self.reason = Some("model context exceeds the configured byte bound".into());
        }
        let Some(ledger) = state.cost_work.get(&run.run_id) else {
            return;
        };
        let mut spent = self.prior_spend;
        let mut unknown = false;
        self.tool_calls = 0;
        for (work_id, work) in &ledger.work {
            match work.kind {
                CostWorkKind::WorkspaceTool => self.tool_calls = self.tool_calls.saturating_add(1),
                CostWorkKind::ProviderAttempt if work.state == CostWorkState::OutcomeRecorded => {
                    if let Some(usage) = &work.generation_usage {
                        if let Some(rates) = self.config.estimate_rates {
                            spent = spent.saturating_add(crate::agent::estimate_cost(
                                usage.prompt_tokens,
                                usage.completion_tokens,
                                rates,
                            ));
                        } else if let Some(amount) = work
                            .token_estimate
                            .as_ref()
                            .and_then(|cost| cost.estimated_micro_usd())
                        {
                            spent = spent.saturating_add(amount);
                        } else {
                            unknown = true;
                        }
                    } else {
                        unknown = true;
                    }
                }
                CostWorkKind::DecisionModel if work.state == CostWorkState::OutcomeRecorded => {
                    if let Some(amount) = work.decision_estimate_micro_usd {
                        spent = spent.saturating_add(amount);
                    } else {
                        unknown |=
                            state
                                .context_store
                                .decisions
                                .get(work_id)
                                .is_none_or(|receipt| {
                                    receipt
                                        .outcome
                                        .as_ref()
                                        .is_some_and(|outcome| match outcome {
                                            Ok(_) => true,
                                            Err(error) => error.may_have_run,
                                        })
                                });
                    }
                }
                _ => {}
            }
        }
        self.spent = spent;
        if let Some(bound) = self.config.max_spend_microusd {
            if unknown {
                self.reason =
                    Some("estimated spend is unavailable for completed model work".into());
            } else if spent >= bound {
                self.reason = Some("estimated spend bound reached".into());
            }
        }
    }

    pub(super) fn resume(
        config: &AgentConfig,
        report: &RunReport,
        state: &SessionSnapshot,
    ) -> Result<Self, String> {
        let run = state.run.as_ref().ok_or("native continuation has no run")?;
        let mut prior = Self::new(config, report);
        prior.prior_spend = 0;
        prior.observe(state);
        let mut budget = Self::new(config, report);
        budget.prior_spend = report
            .estimated_spend_microusd
            .checked_sub(prior.spent)
            .ok_or("native continuation cost baseline regressed")?;
        budget.prior_steps = report
            .steps
            .checked_sub(run.model_attempts)
            .ok_or("native continuation attempt baseline regressed")?;
        budget.prior_tools = report
            .tool_calls
            .checked_sub(prior.tool_calls)
            .ok_or("native continuation tool baseline regressed")?;
        budget.prior_duration = report
            .active_duration_ms
            .checked_sub(run.active_ms)
            .ok_or("native continuation clock baseline regressed")?;
        budget.observe(state);
        Ok(budget)
    }

    pub(super) fn update_report(&self, report: &mut RunReport, state: &SessionSnapshot) {
        report.estimated_spend_microusd = self.spent;
        report.tool_calls = self.prior_tools.saturating_add(self.tool_calls);
        if let Some(run) = &state.run {
            report.steps = self.prior_steps.saturating_add(run.model_attempts);
            report.active_duration_ms = self.prior_duration.saturating_add(run.active_ms);
        }
    }

    pub(super) fn admit(&self) -> Result<(), CoreError> {
        match &self.reason {
            Some(reason) => Err(CoreError::rejected(ErrorCode::LimitExceeded, reason)),
            None if self
                .config
                .max_spend_microusd
                .is_some_and(|bound| self.spent >= bound) =>
            {
                Err(CoreError::rejected(
                    ErrorCode::LimitExceeded,
                    "estimated spend bound reached",
                ))
            }
            None => Ok(()),
        }
    }

    pub(super) fn checkpoint(
        &self,
        state: &SessionSnapshot,
    ) -> Option<crate::store::ExecutionRecord> {
        let run = state.run.as_ref()?;
        let root = state.agents.get(&state.agent_id)?;
        Some(crate::store::ExecutionRecord::RunCheckpoint {
            context_version: root.context_revision,
            messages: root.history.clone(),
            model_steps: self.prior_steps.saturating_add(run.model_attempts),
            tool_calls: self.prior_tools.saturating_add(self.tool_calls),
            estimated_spend_microusd: self.spent,
            active_duration_ms: self.prior_duration.saturating_add(run.active_ms),
        })
    }
}
