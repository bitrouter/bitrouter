//! The `ChargeStrategy` responsibility chain: `ByokCharge` → `CreditCharge` →
//! `MppCharge`. The pipeline tries them in registration order and `break`s on
//! the first `Claimed` — "charge at most once" is a structural guarantee, not
//! hook etiquette (003 §4.5).
//!
//! `CreditCharge` is the only module that touches `credit_accounts` and
//! `credit_ledger_entries`.

use async_trait::async_trait;
use chrono::Utc;
use sqlx::{Row, SqlitePool};

use bitrouter_sdk::caller::{FundingSource, PaymentMethod};
use bitrouter_sdk::language_model::{ChargeOutcome, ChargeStrategy, SettlementContext, Usage};
use bitrouter_sdk::{BitrouterError, Result};

use crate::events::{ByokKeyApplied, MppCheckpointSigned, PricingUnavailable};
use crate::mpp::MppState;
use crate::pricing::{PricingTable, calculate_charge_micro_usd};

// ===== credit_accounts / credit_ledger_entries helpers (owned by CreditCharge) =====

/// Read a user's credit balance (micro-USD); `0` if no account row exists.
pub async fn credit_balance(pool: &SqlitePool, user_id: &str) -> Result<i64> {
    let row = sqlx::query("SELECT balance_micro_usd FROM credit_accounts WHERE user_id = ?")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("credit_balance: {e}")))?;
    Ok(row
        .map(|r| r.get::<i64, _>("balance_micro_usd"))
        .unwrap_or(0))
}

/// Number of ledger entries for a user (used by tests to assert idempotency).
pub async fn credit_ledger_count(pool: &SqlitePool, user_id: &str) -> Result<i64> {
    let row = sqlx::query("SELECT COUNT(*) AS n FROM credit_ledger_entries WHERE user_id = ?")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("credit_ledger_count: {e}")))?;
    Ok(row.get::<i64, _>("n"))
}

/// Credit a user's balance (used by the CLI / tests to top up). Writes a
/// positive ledger entry with a NULL idempotency key (manual top-ups are not
/// request-driven), and bumps the account balance, in one transaction.
pub async fn add_credits(pool: &SqlitePool, user_id: &str, amount: i64) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| BitrouterError::internal(format!("add_credits begin: {e}")))?;
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO credit_ledger_entries \
         (user_id, delta_micro_usd, request_id, idempotency_key, created_at) \
         VALUES (?, ?, NULL, NULL, ?)",
    )
    .bind(user_id)
    .bind(amount)
    .bind(&now)
    .execute(&mut *tx)
    .await
    .map_err(|e| BitrouterError::internal(format!("add_credits ledger: {e}")))?;
    sqlx::query(
        "INSERT INTO credit_accounts (user_id, balance_micro_usd, updated_at) \
         VALUES (?, ?, ?) \
         ON CONFLICT(user_id) DO UPDATE SET \
           balance_micro_usd = balance_micro_usd + excluded.balance_micro_usd, \
           updated_at = excluded.updated_at",
    )
    .bind(user_id)
    .bind(amount)
    .bind(&now)
    .execute(&mut *tx)
    .await
    .map_err(|e| BitrouterError::internal(format!("add_credits balance: {e}")))?;
    tx.commit()
        .await
        .map_err(|e| BitrouterError::internal(format!("add_credits commit: {e}")))?;
    Ok(())
}

/// Deduct `amount` micro-USD from a user's balance, **idempotently** keyed by
/// `idempotency_key` (004 §7.5). The deduction and its ledger entry are written
/// in one transaction; if a ledger row with the same `idempotency_key` already
/// exists the call is a retry — the balance is left untouched.
///
/// The pre-flight balance gate is `BalanceCheckHook` (PreRequest); by settlement
/// time the request has run, so a *first* deduction is applied unconditionally
/// (the balance may go negative — a debt to reconcile) — never a silent skip.
///
/// Returns `true` if the balance was debited, `false` if this was a duplicate.
pub(crate) async fn deduct_credits(
    pool: &SqlitePool,
    user_id: &str,
    amount: i64,
    idempotency_key: &str,
) -> Result<bool> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| BitrouterError::internal(format!("deduct_credits begin: {e}")))?;
    let now = Utc::now().to_rfc3339();

    // `INSERT OR IGNORE` against the UNIQUE idempotency_key — a retry inserts
    // zero rows, and we then leave the balance alone.
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO credit_ledger_entries \
         (user_id, delta_micro_usd, request_id, idempotency_key, created_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(-amount)
    .bind(idempotency_key)
    .bind(idempotency_key)
    .bind(&now)
    .execute(&mut *tx)
    .await
    .map_err(|e| BitrouterError::internal(format!("deduct_credits ledger: {e}")))?;

    if inserted.rows_affected() == 0 {
        // Duplicate — the original request already debited the balance.
        tx.rollback()
            .await
            .map_err(|e| BitrouterError::internal(format!("deduct_credits rollback: {e}")))?;
        tracing::info!(
            user_id,
            idempotency_key,
            "duplicate credit deduction ignored (idempotent retry)"
        );
        return Ok(false);
    }

    sqlx::query(
        "INSERT INTO credit_accounts (user_id, balance_micro_usd, updated_at) \
         VALUES (?, ?, ?) \
         ON CONFLICT(user_id) DO UPDATE SET \
           balance_micro_usd = balance_micro_usd - ?, \
           updated_at = excluded.updated_at",
    )
    .bind(user_id)
    .bind(-amount)
    .bind(&now)
    .bind(amount)
    .execute(&mut *tx)
    .await
    .map_err(|e| BitrouterError::internal(format!("deduct_credits balance: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| BitrouterError::internal(format!("deduct_credits commit: {e}")))?;
    Ok(true)
}

fn usage_of(ctx: &SettlementContext) -> Usage {
    Usage {
        prompt_tokens: ctx.prompt_tokens,
        completion_tokens: ctx.completion_tokens,
        reasoning_tokens: ctx.reasoning_tokens,
    }
}

// ===== ByokCharge — chain link 1 =====

/// First link of the chain. If a `ByokKeyApplied` event is present, the request
/// is BYOK: charge nothing and claim, so neither `CreditCharge` nor `MppCharge`
/// runs. `byok_used` is set **here**, from the event — never reverse-inferred
/// from `api_key_override` (cloud #235).
pub struct ByokCharge;

#[async_trait]
impl ChargeStrategy for ByokCharge {
    async fn try_charge(&self, ctx: &mut SettlementContext) -> Result<ChargeOutcome> {
        if ctx.has_event::<ByokKeyApplied>() {
            ctx.final_charge_micro_usd = 0;
            ctx.funding_source = FundingSource::Byok;
            ctx.byok_used = true;
            return Ok(ChargeOutcome::Claimed);
        }
        Ok(ChargeOutcome::Pass)
    }
}

// ===== CreditCharge — chain link 2 =====

/// Second link: charges credit-paying callers against `credit_accounts`.
/// Passes (does not claim) for non-credits callers so `MppCharge` gets a turn.
pub struct CreditCharge {
    pool: SqlitePool,
    pricing: PricingTable,
}

impl CreditCharge {
    /// Build a `CreditCharge` over a sqlite pool and a pricing table.
    pub fn new(pool: SqlitePool, pricing: PricingTable) -> Self {
        Self { pool, pricing }
    }
}

#[async_trait]
impl ChargeStrategy for CreditCharge {
    async fn try_charge(&self, ctx: &mut SettlementContext) -> Result<ChargeOutcome> {
        if ctx.caller.payment_method() != PaymentMethod::Credits {
            return Ok(ChargeOutcome::Pass);
        }

        // #180 / #440 / #443: a missing price is "unconfigured", not "free".
        // Per 004 §1.5, an unconfigured price means **`Pass`, not `Claim`** —
        // the charge is left unsettled (funding_source stays `Unsettled`), a
        // `PricingUnavailable` event is emitted, and zero is never silently
        // debited from a real account.
        match self.pricing.resolve(&ctx.provider_id, &ctx.model_id) {
            Some(pricing) if !pricing.is_unconfigured() => {
                let charge = calculate_charge_micro_usd(&usage_of(ctx), &pricing);
                // Idempotent on request_id — a retried settlement of the same
                // request never double-debits (004 §7.5).
                deduct_credits(&self.pool, ctx.caller.user_id(), charge, &ctx.request_id).await?;
                ctx.final_charge_micro_usd = charge;
                ctx.funding_source = FundingSource::Credits;
                Ok(ChargeOutcome::Claimed)
            }
            _ => {
                tracing::warn!(
                    provider = %ctx.provider_id,
                    service_id = %ctx.model_id,
                    "no pricing configured for target — skipping credit charge"
                );
                ctx.emit(PricingUnavailable {
                    provider_id: ctx.provider_id.clone(),
                    service_id: ctx.model_id.clone(),
                });
                Ok(ChargeOutcome::Pass)
            }
        }
    }
}

// ===== MppCharge — chain link 3 =====

/// Third link: settles MPP-paying callers against an MPP channel. v1.0 delivers
/// the **Tempo** channel only; Solana is a placeholder feature, not wired
/// (008 §1.1).
pub struct MppCharge {
    state: MppState,
    pricing: PricingTable,
}

impl MppCharge {
    /// Build an `MppCharge` over an `MppState` and a pricing table.
    pub fn new(state: MppState, pricing: PricingTable) -> Self {
        Self { state, pricing }
    }
}

#[async_trait]
impl ChargeStrategy for MppCharge {
    async fn try_charge(&self, ctx: &mut SettlementContext) -> Result<ChargeOutcome> {
        if ctx.caller.payment_method() != PaymentMethod::Mpp {
            return Ok(ChargeOutcome::Pass);
        }

        // Per 004 §1.6: an unconfigured price → emit `PricingUnavailable` and
        // `Pass` (do not settle), exactly as `CreditCharge` does.
        let charge = match self.pricing.resolve(&ctx.provider_id, &ctx.model_id) {
            Some(pricing) if !pricing.is_unconfigured() => {
                calculate_charge_micro_usd(&usage_of(ctx), &pricing)
            }
            _ => {
                tracing::warn!(
                    provider = %ctx.provider_id,
                    service_id = %ctx.model_id,
                    "no pricing configured for target — skipping MPP charge"
                );
                ctx.emit(PricingUnavailable {
                    provider_id: ctx.provider_id.clone(),
                    service_id: ctx.model_id.clone(),
                });
                return Ok(ChargeOutcome::Pass);
            }
        };

        // Streaming requests settle incrementally via `MppStreamHook`, which
        // emits `MppCheckpointSigned` events; the final reconciliation here
        // charges only the remainder not yet checkpointed. The last checkpoint
        // event's cumulative value is what was already settled.
        let already = ctx
            .get_events::<MppCheckpointSigned>()
            .last()
            .map(|e| e.cumulative_micro_usd)
            .unwrap_or(0);
        let remainder = (charge - already).max(0);
        self.state.settle(ctx.caller.user_id(), remainder).await?;
        ctx.final_charge_micro_usd = charge;
        ctx.funding_source = FundingSource::Mpp;
        Ok(ChargeOutcome::Claimed)
    }
}
