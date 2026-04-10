//! Pre-request budget enforcement.
//!
//! Compares the caller's accumulated spend (from the database) against the
//! `bgt` claim in their JWT. If the accumulated spend meets or exceeds the
//! budget, the request is rejected with HTTP 429.
//!
//! **Soft-margin**: the request that *crosses* the threshold is allowed
//! through — only subsequent requests are rejected. This avoids the
//! complexity of pre-estimating request cost.

use std::sync::Arc;

use bitrouter_core::auth::claims::BudgetScope;
use bitrouter_core::observe::CallerContext;
use bitrouter_observe::spend::store::SpendStore;

/// Check the caller's budget before routing the request.
///
/// Returns the `CallerContext` unchanged when the budget is not exhausted
/// (or when no budget is configured). Rejects with [`BudgetExhausted`]
/// when accumulated spend meets or exceeds the budget.
pub async fn check_budget(
    caller: CallerContext,
    spend_store: Arc<dyn SpendStore>,
) -> Result<CallerContext, warp::Rejection> {
    let (budget, scope) = match (caller.budget, caller.budget_scope) {
        (Some(b), Some(s)) => (b, s),
        // No budget claim → pass through without enforcement.
        _ => return Ok(caller),
    };

    let account_id = match caller.account_id.as_deref() {
        Some(id) => id,
        // Anonymous callers cannot have budgets.
        None => return Ok(caller),
    };

    // Compute the `since` boundary based on scope.
    let since = match scope {
        BudgetScope::Account => None,
        BudgetScope::Session => {
            // Use the JWT issued-at time as the session start boundary.
            caller.issued_at.and_then(|ts| {
                chrono::DateTime::from_timestamp(ts as i64, 0).map(|dt| dt.naive_utc())
            })
        }
    };

    let accumulated_usd = spend_store.query_total_spend(account_id, since, None).await;

    // Convert budget from micro-USD to USD for comparison.
    let budget_usd = budget as f64 / 1_000_000.0;

    if accumulated_usd >= budget_usd {
        return Err(warp::reject::custom(BudgetExhausted {
            budget_usd,
            accumulated_usd,
            scope,
        }));
    }

    Ok(caller)
}

/// Rejection type for exhausted budgets.
#[derive(Debug)]
pub struct BudgetExhausted {
    pub budget_usd: f64,
    pub accumulated_usd: f64,
    pub scope: BudgetScope,
}

impl std::fmt::Display for BudgetExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scope_label = match self.scope {
            BudgetScope::Session => "session",
            BudgetScope::Account => "account",
        };
        write!(
            f,
            "budget exhausted: {scope_label} spend ${:.6} >= budget ${:.6}",
            self.accumulated_usd, self.budget_usd,
        )
    }
}

impl warp::reject::Reject for BudgetExhausted {}
