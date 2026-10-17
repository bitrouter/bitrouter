//! One Goal's cumulative limits and conservative, retained token reservations.
//! Unknown request bills, harness fees and tools are outside this known scope.

use bitrouter_sdk::language_model::native_accounting::NativeTokenReservation;
use serde::{Deserialize, Serialize};

use super::{GoalError, GoalSnapshot, identifier, invalid, message};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Total elapsed time since Goal creation, including waits and pauses.
    pub elapsed_ms: Option<u64>,
    /// Configured estimates only; never an invoice/all-external-cost guarantee.
    pub managed_token_estimates_micro_usd: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub limits: Limits,
    pub elapsed_ms: u64,
    pub held: Vec<Reservation>,
    pub halt: Option<String>,
    pub resume_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub work_id: String,
    pub quote: NativeTokenReservation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    ObserveElapsed,
    Halt { reason: String },
}

impl Budget {
    pub fn elapsed(&self, created_at_ms: u64, now_ms: u64) -> u64 {
        self.elapsed_ms.max(now_ms.saturating_sub(created_at_ms))
    }

    pub fn admission(&self, created_at_ms: u64, now_ms: u64) -> Result<(), GoalError> {
        if self.resume_required
            || self
                .limits
                .elapsed_ms
                .is_some_and(|limit| self.elapsed(created_at_ms, now_ms) >= limit)
        {
            return Err(exceeded(
                "Goal budget reached; explicit budget update and resume are required",
            ));
        }
        Ok(())
    }

    pub fn reserved_micro_usd(&self) -> Result<u64, GoalError> {
        self.held.iter().try_fold(0_u64, |total, held| {
            total
                .checked_add(held.quote.micro_usd)
                .ok_or_else(|| invalid("reservation total exhausted"))
        })
    }

    pub fn can_reserve(
        &self,
        spent: u64,
        quote: Option<&NativeTokenReservation>,
    ) -> Result<(), GoalError> {
        if let Some(limit) = self.limits.managed_token_estimates_micro_usd {
            let quote = quote.ok_or_else(|| exceeded("This model call has no bounded token-price quote; it cannot enter the configured estimate cap"))?;
            let exposure = spent
                .checked_add(self.reserved_micro_usd()?)
                .and_then(|total| total.checked_add(quote.micro_usd))
                .ok_or_else(|| exceeded("Goal token-price exposure overflow"))?;
            if exposure > limit {
                return Err(exceeded(
                    "Insufficient remaining Goal token-estimate budget for this reservation",
                ));
            }
        }
        Ok(())
    }
}

/// A current worker may wait for other current workers' in-flight quotes.
/// Settled calls with missing usage retain their bound and cannot release it
/// merely because another worker wants to proceed.
pub fn waiting_on_workers(
    goal: &GoalSnapshot,
    quote: Option<&NativeTokenReservation>,
) -> Result<bool, GoalError> {
    let spent = goal.accounting.estimated_model_tokens_micro_usd;
    if goal.budget.can_reserve(spent, quote).is_ok() {
        return Ok(false);
    }
    let mut available = goal.budget.clone();
    available.held.retain(|held| {
        !goal.accounting.in_flight.iter().any(|work| {
            work.id == held.work_id
                && work
                    .source
                    .as_ref()
                    .is_some_and(|source| super::workflow::worker_requested(goal, source))
                && work.kind == super::accounting::WorkKind::ProviderAttempt
        })
    });
    available.can_reserve(spent, quote)?;
    Ok(true)
}

pub(super) fn apply(goal: &mut GoalSnapshot, event: &Event, now_ms: u64) -> Result<(), GoalError> {
    match event {
        Event::ObserveElapsed => {
            goal.budget.elapsed_ms = goal.budget.elapsed(goal.created_at_ms, now_ms);
            if goal
                .budget
                .limits
                .elapsed_ms
                .is_some_and(|limit| goal.budget.elapsed_ms >= limit)
            {
                goal.budget.resume_required = true;
                goal.budget.halt = Some(
                    "Total elapsed Goal time reached its limit, including waits and pauses".into(),
                );
            }
        }
        Event::Halt { reason } => {
            message(reason)?;
            goal.budget.resume_required = true;
            goal.budget.halt = Some(reason.clone());
        }
    }
    Ok(())
}

pub(super) fn reserve(
    goal: &mut GoalSnapshot,
    work_id: &str,
    quote: Option<&NativeTokenReservation>,
    priced_work: bool,
    now_ms: u64,
) -> Result<(), GoalError> {
    goal.budget.admission(goal.created_at_ms, now_ms)?;
    if !priced_work {
        return Ok(());
    }
    goal.budget
        .can_reserve(goal.accounting.estimated_model_tokens_micro_usd, quote)?;
    if let Some(quote) = quote {
        identifier(&quote.pricing_version)?;
        identifier(&quote.provider)?;
        if quote.model.is_empty()
            || quote.model.len() > 4096
            || goal.budget.held.len() >= 256
            || goal.budget.held.iter().any(|held| held.work_id == work_id)
        {
            return Err(invalid("invalid or duplicate Goal cost reservation"));
        }
        goal.budget.held.push(Reservation {
            work_id: work_id.into(),
            quote: quote.clone(),
        });
    }
    Ok(())
}

pub(super) fn settle(
    goal: &mut GoalSnapshot,
    work_id: &str,
    estimate: Option<u64>,
    verified: bool,
) {
    let index = goal
        .budget
        .held
        .iter()
        .position(|held| held.work_id == work_id);
    if let Some(index) = index
        && let Some(estimate) = estimate
    {
        let held = goal.budget.held.remove(index);
        if !verified || estimate > held.quote.micro_usd {
            goal.budget.resume_required = true;
            goal.budget.halt = Some("Actual token estimate or serving evidence exceeded/differed from its frozen reservation".into());
        }
    }
    // Missing usage retains the entire bound after the effect has settled.
    // A cancelled/failed provider call is not known to have cost zero.
}

fn exceeded(message: &str) -> GoalError {
    GoalError::BudgetExceeded {
        message: message.into(),
    }
}
