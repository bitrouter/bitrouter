//! Shared admission and provider execution state transitions.

mod admission;
mod attempts;
mod finalization;
mod lease;
pub(super) mod management;
mod outbox;
mod recovery;
pub(super) mod resolution;

use std::collections::BTreeSet;

use super::credentials::CredentialStamp;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{GatewayAttempt, GatewayExecution, GatewayTerminalEvent};
use bitrouter_ai::types::ModelOperation;
use sea_orm::DatabaseTransaction;
use serde::{Deserialize, Serialize};

use super::authority::{Authority, Revision};
use super::store::{Kind, NewRecord, Store, Stored};

/// Stable classifications exposed by HTTP without backend diagnostics.
#[derive(Debug, thiserror::Error)]
pub(super) enum Rejection {
    #[error("shared gateway authentication failed")]
    Unauthorized,
    #[error("shared gateway resource scope denies this request")]
    Forbidden,
    #[error("shared gateway quota is exhausted")]
    Quota,
    #[error("idempotency key is already bound to a different request")]
    IdempotencyConflict,
    #[error("shared gateway authority changed before dispatch")]
    AuthorityChanged,
    #[error("shared gateway worker is not accepting work")]
    Unavailable,
    #[error("shared gateway execution was not found")]
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TenantLimits {
    pub concurrent_executions: u32,
    pub requests_per_minute: u32,
    pub resource_pools: BTreeSet<String>,
}

impl TenantLimits {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.concurrent_executions <= 1_000_000 && self.requests_per_minute <= 1_000_000,
            "shared tenant limit exceeds supported bound"
        );
        ensure!(
            self.resource_pools.len() <= 256,
            "shared tenant has too many resource pools"
        );
        for pool in &self.resource_pools {
            validate_id(pool)?;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tenant {
    limits: TenantLimits,
    policy_revision: u64,
    active: u32,
    minute: i64,
    accepted: u32,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RateBucket {
    minute: i64,
    accepted: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resource {
    concurrent_attempts: u32,
    policy_revision: u64,
    active: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attempt {
    execution_id: String,
    credential: CredentialStamp,
    resource_revision: u64,
    view: GatewayAttempt,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Instance {
    expires_at_ms: i64,
    accepting: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Execution {
    tenant_id: String,
    principal_id: String,
    authorization_revision: String,
    tenant_revision: u64,
    owner: String,
    trajectory: Option<TrajectoryBinding>,
    view: GatewayExecution,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TrajectoryBinding {
    pub owner_user_id: String,
    pub episode_id: String,
    pub request_id: String,
    pub correlation_epoch: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EpisodeFence {
    owner_user_id: String,
    episode_id: String,
    pending: BTreeSet<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Outbox {
    event: GatewayTerminalEvent,
    lease: Option<OutboxLease>,
    deliveries: u32,
    acknowledged_at_ms: Option<i64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutboxLease {
    token: String,
    expires_at_ms: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Idempotency {
    tenant_id: String,
    principal_id: String,
    fingerprint: String,
    operation: ModelOperation,
    execution_id: String,
}

/// Raw credentials and request content never enter durable admission records.
pub(super) struct Admission<'a> {
    pub key_hash: &'a str,
    pub correlation_id: &'a str,
    pub idempotency_key: Option<&'a str>,
    pub fingerprint: &'a str,
    pub operation: ModelOperation,
    pub revision: Revision,
}

pub(super) enum AdmissionResult {
    Accepted(GatewayExecution, crate::auth::events::Authenticated),
    Existing(GatewayExecution),
}

#[derive(Clone)]
pub(super) struct Engine {
    authority: Authority,
    /// Random incarnation identity. Reusing a human display name cannot reclaim work.
    owner: String,
    lease_ms: i64,
}

/// Renewal-only capability for the original incarnation on a dedicated pool.
pub(super) struct Heartbeat {
    engine: Engine,
}

/// Database-clock evidence from a committed renewal, not a readiness assertion.
#[derive(Debug)]
pub(super) struct LeaseObservation {
    pub database_now_ms: i64,
    pub expires_at_ms: i64,
    pub accepting: bool,
}

impl Engine {
    pub async fn join(authority: Authority, lease_ms: u64) -> Result<Self> {
        ensure!(
            (1_000..=300_000).contains(&lease_ms),
            "shared lease must be 1..300 seconds"
        );
        let engine = Self {
            authority,
            owner: uuid::Uuid::new_v4().to_string(),
            lease_ms: i64::try_from(lease_ms)?,
        };
        engine
            .store()
            .retry(|| async {
                let tx = engine.store().begin().await?;
                engine.authority.fence(&tx).await?;
                let now = Store::now(&tx).await?;
                ensure!(
                    engine
                        .store()
                        .create(
                            &tx,
                            NewRecord {
                                kind: Kind::Instance,
                                key: &engine.owner,
                                status: "live",
                                due_at_ms: Some(engine.deadline(now)?),
                                now_ms: now,
                                value: &Instance {
                                    expires_at_ms: engine.deadline(now)?,
                                    accepting: true
                                },
                            }
                        )
                        .await?,
                    "shared worker incarnation already exists"
                );
                tx.commit().await?;
                Ok(())
            })
            .await?;
        Ok(engine)
    }

    fn store(&self) -> &Store {
        self.authority.store()
    }

    pub fn namespace(&self) -> &str {
        self.store().namespace()
    }

    fn deadline(&self, now: i64) -> Result<i64> {
        now.checked_add(self.lease_ms)
            .context("shared lease clock overflow")
    }

    pub async fn drain(&self) -> Result<()> {
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                let mut instance: Stored<Instance> =
                    self.store().lock(&tx, Kind::Instance, &self.owner).await?;
                let now = Store::now(&tx).await?;
                instance.value.accepting = false;
                let due = instance.value.expires_at_ms;
                self.store()
                    .save(&tx, &mut instance, "draining", Some(due), now)
                    .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    async fn live_owner(&self, tx: &DatabaseTransaction, admission: bool) -> Result<i64> {
        let instance: Stored<Instance> = self
            .store()
            .lock_shared(tx, Kind::Instance, &self.owner)
            .await?;
        let now = Store::now(tx).await?;
        ensure!(
            instance.value.expires_at_ms > now && (!admission || instance.value.accepting),
            Rejection::Unavailable
        );
        Ok(instance.value.expires_at_ms)
    }

    /// Limits may fall below occupancy; they never erase existing reservations.
    pub async fn set_tenant(
        &self,
        tenant_id: &str,
        expected_revision: Option<u64>,
        limits: &TenantLimits,
    ) -> Result<u64> {
        let input = bitrouter_ai::gateway::operator::TenantPut {
            tenant_id: tenant_id.into(),
            expected_revision,
            limits: bitrouter_ai::gateway::operator::TenantLimits {
                concurrent_executions: limits.concurrent_executions,
                requests_per_minute: limits.requests_per_minute,
                resource_pools: limits.resource_pools.clone(),
            },
        };
        management::validate_tenant(&input)?;
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                self.authority.fence(&tx).await?;
                let policy = management::put_tenant(&self.authority, &tx, &input).await?;
                tx.commit().await?;
                Ok(policy.revision)
            })
            .await
    }
}

pub(super) fn validate_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "shared identity must contain 1..256 bytes without control characters"
    );
    Ok(())
}
