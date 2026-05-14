//! MPP (micropayment-protocol) settlement.
//!
//! v1.0 delivers the **Tempo** channel only. Solana MPP sessions are explicitly
//! out of scope for v1.0 (008 §1.1) — the `mpp-solana` feature is a placeholder
//! that is never wired; constructing an `MppState` for the Solana channel is an
//! error.
//!
//! `MppState` is the only module that touches the `mpp_sessions` table.
//!
//! NOTE: the Tempo EVM wallet + channel close-signing (cloud #183, still OPEN)
//! is a known follow-up — it is *not* a direct migration. `MppState` here
//! tracks channel balance and per-checkpoint progress in `mpp_sessions`; the
//! on-chain signing path plugs into [`MppState::sign_checkpoint`].

use chrono::Utc;
use sqlx::{Row, SqlitePool};

use bitrouter_sdk::{BitrouterError, MppVerification, MppVerifier, Result};

/// Which MPP payment channel a session uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MppChannel {
    /// The Tempo EVM channel — the only channel delivered in v1.0.
    Tempo,
}

/// Shared MPP channel state, backed by the `mpp_sessions` table.
#[derive(Clone)]
pub struct MppState {
    pool: SqlitePool,
    channel: MppChannel,
}

impl MppState {
    /// Build an `MppState` for the Tempo channel.
    pub fn tempo(pool: SqlitePool) -> Self {
        Self {
            pool,
            channel: MppChannel::Tempo,
        }
    }

    /// Build an `MppState` for the Solana channel — **unsupported in v1.0**.
    /// Solana MPP sessions are out of scope (008 §1.1); this always errors so a
    /// misconfiguration fails loudly rather than silently mis-settling.
    pub fn solana(_pool: SqlitePool) -> Result<Self> {
        Err(BitrouterError::internal(
            "Solana MPP channel is not supported in v1.0 (008 §1.1) — \
             the mpp-solana feature is a placeholder",
        ))
    }

    /// The channel this state settles against.
    pub fn channel(&self) -> MppChannel {
        self.channel
    }

    /// Open (or top up) an MPP session with a starting channel balance. Used by
    /// the auth/MPP verification path and by tests.
    pub async fn open_session(
        &self,
        session_id: &str,
        user_id: &str,
        balance_micro_usd: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO mpp_sessions \
             (session_id, user_id, channel, balance_micro_usd, \
              last_checkpoint_micro_usd, updated_at) \
             VALUES (?, ?, ?, ?, 0, ?) \
             ON CONFLICT(session_id) DO UPDATE SET \
               balance_micro_usd = balance_micro_usd + excluded.balance_micro_usd, \
               updated_at = excluded.updated_at",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(channel_str(self.channel))
        .bind(balance_micro_usd)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("open mpp session: {e}")))?;
        Ok(())
    }

    /// The most recent MPP session id for a user, if any.
    pub async fn session_for_user(&self, user_id: &str) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT session_id FROM mpp_sessions WHERE user_id = ? \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("mpp session lookup: {e}")))?;
        Ok(row.map(|r| r.get("session_id")))
    }

    /// The channel balance (micro-USD) of a session; `0` if unknown.
    pub async fn balance(&self, session_id: &str) -> Result<i64> {
        let row = sqlx::query("SELECT balance_micro_usd FROM mpp_sessions WHERE session_id = ?")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| BitrouterError::internal(format!("mpp balance: {e}")))?;
        Ok(row
            .map(|r| r.get::<i64, _>("balance_micro_usd"))
            .unwrap_or(0))
    }

    /// Look up a session's `(user_id, balance)` — `None` if no such session.
    pub async fn session(&self, session_id: &str) -> Result<Option<(String, i64)>> {
        let row =
            sqlx::query("SELECT user_id, balance_micro_usd FROM mpp_sessions WHERE session_id = ?")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| BitrouterError::internal(format!("mpp session: {e}")))?;
        Ok(row.map(|r| (r.get("user_id"), r.get::<i64, _>("balance_micro_usd"))))
    }

    /// Sign a streaming checkpoint: advance the session's
    /// `last_checkpoint_micro_usd` to `cumulative` and debit the channel
    /// balance by the delta since the previous checkpoint. Idempotent on a
    /// non-advancing `cumulative`.
    ///
    /// The on-chain Tempo signature is produced here in the full
    /// implementation; v1.0 records channel progress and leaves the signing
    /// hook as the documented follow-up.
    pub async fn sign_checkpoint(
        &self,
        session_id: &str,
        cumulative_micro_usd: i64,
    ) -> Result<i64> {
        let prev =
            sqlx::query("SELECT last_checkpoint_micro_usd FROM mpp_sessions WHERE session_id = ?")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| BitrouterError::internal(format!("mpp checkpoint read: {e}")))?
                .map(|r| r.get::<i64, _>("last_checkpoint_micro_usd"))
                .unwrap_or(0);

        let delta = (cumulative_micro_usd - prev).max(0);
        if delta == 0 {
            return Ok(0);
        }
        sqlx::query(
            "UPDATE mpp_sessions SET \
               last_checkpoint_micro_usd = ?, \
               balance_micro_usd = balance_micro_usd - ?, \
               updated_at = ? \
             WHERE session_id = ?",
        )
        .bind(cumulative_micro_usd)
        .bind(delta)
        .bind(Utc::now().to_rfc3339())
        .bind(session_id)
        .execute(&self.pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("mpp checkpoint write: {e}")))?;
        Ok(delta)
    }

    /// Settle a final amount against a user's MPP session (the non-streaming
    /// path, or the streaming remainder not yet checkpointed). A no-op when the
    /// user has no session or `amount` is zero.
    pub async fn settle(&self, user_id: &str, amount: i64) -> Result<()> {
        if amount <= 0 {
            return Ok(());
        }
        let Some(session_id) = self.session_for_user(user_id).await? else {
            tracing::warn!(%user_id, "MPP settle: no session for user — skipping");
            return Ok(());
        };
        sqlx::query(
            "UPDATE mpp_sessions SET \
               balance_micro_usd = balance_micro_usd - ?, \
               last_checkpoint_micro_usd = last_checkpoint_micro_usd + ?, \
               updated_at = ? \
             WHERE session_id = ?",
        )
        .bind(amount)
        .bind(amount)
        .bind(Utc::now().to_rfc3339())
        .bind(&session_id)
        .execute(&self.pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("mpp settle: {e}")))?;
        Ok(())
    }
}

/// Extract the channel session id from a `Payment-SIGNATURE` header value.
///
/// v1.0 accepts either a bare session id or a `key=value;…` credential string
/// carrying `session=<id>` (and, in the full implementation, `sig=<voucher>`).
/// **The on-chain voucher signature check is the documented Tempo follow-up**
/// (cloud #183) — v1.0 resolves the credential to a known, positive-balance
/// channel; it does not yet cryptographically verify the voucher.
fn parse_session_id(credential: &str) -> Option<String> {
    let credential = credential.trim();
    if credential.is_empty() {
        return None;
    }
    for field in credential.split(';') {
        if let Some(value) = field.trim().strip_prefix("session=") {
            return Some(value.trim().to_string());
        }
    }
    // no structured field — treat the whole value as the session id
    if credential.contains('=') {
        None
    } else {
        Some(credential.to_string())
    }
}

#[async_trait]
impl MppVerifier for MppState {
    async fn verify(&self, credential: &str) -> Result<Option<MppVerification>> {
        let Some(session_id) = parse_session_id(credential) else {
            return Ok(None);
        };
        // NOTE: v1.0 resolves the credential to a known channel and checks the
        // balance; cryptographic verification of the Tempo voucher signature is
        // the documented follow-up (see `parse_session_id`).
        match self.session(&session_id).await? {
            Some((user_id, balance)) if balance > 0 => Ok(Some(MppVerification {
                session_id,
                user_id,
                channel_balance_micro_usd: balance,
            })),
            // session exists but is drained, or no such session → not verified
            _ => Ok(None),
        }
    }
}

fn channel_str(c: MppChannel) -> &'static str {
    match c {
        MppChannel::Tempo => "tempo",
    }
}

// ===== MppStreamHook — per-checkpoint streaming settlement =====

use async_trait::async_trait;

use bitrouter_sdk::language_model::{
    StreamAction, StreamContext, StreamHook, StreamInterest, StreamOutcome, StreamPart,
};

use crate::events::MppCheckpointSigned;
use crate::pricing::{PricingTable, calculate_charge_micro_usd};

/// A `language_model::StreamHook` that settles an MPP session incrementally as
/// a stream is delivered. Each `Usage` part advances a signed channel
/// checkpoint, so a mid-stream client disconnect still settles the tokens
/// already delivered — neither over- nor under-charging (003 §4.4 / 008 §3.5).
pub struct MppStreamHook {
    state: MppState,
    pricing: PricingTable,
}

impl MppStreamHook {
    /// Build an `MppStreamHook` over an `MppState` and a pricing table.
    pub fn new(state: MppState, pricing: PricingTable) -> Self {
        Self { state, pricing }
    }

    /// Cumulative cost (micro-USD) of the usage accumulated so far on `ctx`,
    /// or `None` when no pricing is configured for the target.
    fn cumulative_cost(&self, ctx: &StreamContext) -> Option<i64> {
        let usage = ctx.accumulated_usage.finalized()?;
        let target = ctx.target.as_ref()?;
        let pricing = self
            .pricing
            .resolve(&target.provider_name, &target.service_id)?;
        if pricing.is_unconfigured() {
            return None;
        }
        Some(calculate_charge_micro_usd(&usage, &pricing))
    }

    /// Sign a checkpoint for the current cumulative cost and announce it.
    async fn checkpoint(&self, ctx: &mut StreamContext) -> bitrouter_sdk::Result<()> {
        let Some(cumulative) = self.cumulative_cost(ctx) else {
            return Ok(());
        };
        let user_id = ctx.caller.user_id().to_string();
        if let Some(session_id) = self.state.session_for_user(&user_id).await? {
            let delta = self.state.sign_checkpoint(&session_id, cumulative).await?;
            if delta > 0 {
                ctx.emit(MppCheckpointSigned {
                    session_id,
                    cumulative_micro_usd: cumulative,
                });
            }
        }
        Ok(())
    }
}

#[async_trait]
impl StreamHook for MppStreamHook {
    fn interest(&self) -> StreamInterest {
        // Only `Usage` and `Finish` parts matter for incremental settlement —
        // the per-token text hot path never wakes this hook.
        StreamInterest::none().with_usage().with_finish()
    }

    async fn on_part(
        &self,
        ctx: &mut StreamContext,
        part: StreamPart,
    ) -> bitrouter_sdk::Result<StreamAction> {
        // A `Usage` part means new tokens have been delivered — advance the
        // signed checkpoint. The accumulator was already updated by the
        // processor before this hook ran.
        if matches!(part, StreamPart::Usage { .. }) {
            self.checkpoint(ctx).await?;
        }
        Ok(StreamAction::Pass)
    }

    async fn on_stream_end(
        &self,
        ctx: &mut StreamContext,
        _outcome: &StreamOutcome,
    ) -> bitrouter_sdk::Result<()> {
        // Final checkpoint — fires for every termination path (normal end,
        // client disconnect, abort, upstream error), so delivered tokens are
        // always settled.
        self.checkpoint(ctx).await
    }
}
