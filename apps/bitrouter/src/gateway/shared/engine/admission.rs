//! Atomic authenticated tenant admission and idempotency binding.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{ExecutionState, GatewayExecution};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect};
use sha2::{Digest, Sha256};

use crate::auth::entities::api_keys;

use super::{
    Admission, AdmissionResult, Engine, Execution, Idempotency, RateBucket, Rejection, Tenant,
    validate_id,
};
use crate::gateway::shared::store::{Kind, NewRecord, Store, Stored};

pub(super) struct Principal {
    pub(super) id: String,
    pub(super) tenant: String,
    pub(super) revision: String,
    rpm: Option<u32>,
    pub(super) valid_until_ms: Option<i64>,
}

impl Engine {
    pub async fn admit(&self, request: &Admission<'_>) -> Result<AdmissionResult> {
        validate_id(request.correlation_id)?;
        validate_digest(request.key_hash)?;
        validate_digest(request.fingerprint)?;
        if let Some(key) = request.idempotency_key {
            validate_id(key)?;
        }
        // Retry only the transaction, using one stable server-generated identity.
        let execution_id = uuid::Uuid::new_v4().to_string();
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                let head = self.authority.fence(&tx).await?;
                let principal = authenticate(&tx, request.key_hash).await?;
                let mut tenant: Stored<Tenant> = self
                    .store()
                    .lock(&tx, Kind::Tenant, &principal.tenant)
                    .await?;
                let now = Store::now(&tx).await?;
                ensure!(
                    principal.valid_until_ms.is_none_or(|expiry| expiry > now),
                    Rejection::Unauthorized
                );
                let idempotency = request
                    .idempotency_key
                    .map(|key| idempotency_id(&principal.tenant, key))
                    .transpose()?;
                if let Some(key) = &idempotency
                    && let Some(existing) = self
                        .store()
                        .read::<Idempotency>(&tx, Kind::Idempotency, key)
                        .await?
                {
                    ensure!(
                        existing.value.tenant_id == principal.tenant
                            && existing.value.principal_id == principal.id
                            && existing.value.fingerprint == request.fingerprint
                            && existing.value.operation == request.operation,
                        Rejection::IdempotencyConflict
                    );
                    let execution: Stored<Execution> = self
                        .store()
                        .read(&tx, Kind::Execution, &existing.value.execution_id)
                        .await?
                        .context("idempotency execution is missing")?;
                    ensure!(
                        execution.value.tenant_id == principal.tenant
                            && execution.value.principal_id == principal.id,
                        Rejection::Forbidden
                    );
                    let view = project(execution)?;
                    tx.commit().await?;
                    return Ok(AdmissionResult::Existing(view));
                }
                ensure!(
                    head.value.config_generation == request.config_generation,
                    Rejection::AuthorityChanged
                );
                let due = self.live_owner(&tx, true).await?;
                ensure!(
                    tenant.value.active < tenant.value.limits.concurrent_executions,
                    Rejection::Quota
                );
                let minute = now.div_euclid(60_000);
                if tenant.value.minute != minute {
                    tenant.value.minute = minute;
                    tenant.value.accepted = 0;
                }
                ensure!(
                    tenant.value.accepted < tenant.value.limits.requests_per_minute,
                    Rejection::Quota
                );
                if let Some(limit) = principal.rpm {
                    self.store()
                        .create(
                            &tx,
                            NewRecord {
                                kind: Kind::Principal,
                                key: &principal.id,
                                status: "active",
                                due_at_ms: None,
                                now_ms: now,
                                value: &RateBucket {
                                    minute,
                                    accepted: 0,
                                },
                            },
                        )
                        .await?;
                    let mut bucket: Stored<RateBucket> = self
                        .store()
                        .lock(&tx, Kind::Principal, &principal.id)
                        .await?;
                    if bucket.value.minute != minute {
                        bucket.value.minute = minute;
                        bucket.value.accepted = 0;
                    }
                    ensure!(bucket.value.accepted < limit, Rejection::Quota);
                    bucket.value.accepted = bucket
                        .value
                        .accepted
                        .checked_add(1)
                        .context("principal rate overflow")?;
                    self.store()
                        .save(&tx, &mut bucket, "active", None, now)
                        .await?;
                }
                tenant.value.active = tenant
                    .value
                    .active
                    .checked_add(1)
                    .context("tenant concurrency overflow")?;
                tenant.value.accepted = tenant
                    .value
                    .accepted
                    .checked_add(1)
                    .context("tenant rate overflow")?;
                self.store()
                    .save(&tx, &mut tenant, "active", None, now)
                    .await?;
                let view = GatewayExecution {
                    execution_id: execution_id.clone(),
                    correlation_id: request.correlation_id.into(),
                    revision: 0,
                    operation: request.operation,
                    config_generation: request.config_generation,
                    accepted_at_ms: now,
                    updated_at_ms: now,
                    state: ExecutionState::Accepted,
                    attempts: Vec::new(),
                };
                let execution = Execution {
                    tenant_id: principal.tenant.clone(),
                    principal_id: principal.id.clone(),
                    authorization_revision: principal.revision,
                    tenant_revision: tenant.value.policy_revision,
                    owner: self.owner.clone(),
                    trajectory: None,
                    view: view.clone(),
                };
                ensure!(
                    self.store()
                        .create(
                            &tx,
                            NewRecord {
                                kind: Kind::Execution,
                                key: &execution_id,
                                status: "accepted",
                                due_at_ms: Some(due),
                                now_ms: now,
                                value: &execution,
                            }
                        )
                        .await?,
                    "execution identity collision"
                );
                if let Some(key) = idempotency {
                    ensure!(
                        self.store()
                            .create(
                                &tx,
                                NewRecord {
                                    kind: Kind::Idempotency,
                                    key: &key,
                                    status: "bound",
                                    due_at_ms: None,
                                    now_ms: now,
                                    value: &Idempotency {
                                        tenant_id: principal.tenant,
                                        principal_id: principal.id,
                                        fingerprint: request.fingerprint.into(),
                                        operation: request.operation,
                                        execution_id: execution_id.clone()
                                    },
                                }
                            )
                            .await?,
                        Rejection::IdempotencyConflict
                    );
                }
                tx.commit().await?;
                Ok(AdmissionResult::Accepted(view))
            })
            .await
    }

    /// Fresh authentication and tenant ownership apply on every status read.
    pub async fn query(&self, key_hash: &str, execution_id: &str) -> Result<GatewayExecution> {
        validate_digest(key_hash)?;
        validate_id(execution_id)?;
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                self.authority.fence(&tx).await?;
                let principal = authenticate(&tx, key_hash).await?;
                let execution: Stored<Execution> = self
                    .store()
                    .read(&tx, Kind::Execution, execution_id)
                    .await?
                    .ok_or(Rejection::NotFound)?;
                ensure!(
                    execution.value.tenant_id == principal.tenant,
                    Rejection::NotFound
                );
                let view = project(execution)?;
                tx.commit().await?;
                Ok(view)
            })
            .await
    }
}

fn project(execution: Stored<Execution>) -> Result<GatewayExecution> {
    let revision = execution.revision()?;
    let mut view = execution.value.view;
    view.revision = revision;
    Ok(view)
}

pub(super) async fn authenticate(
    tx: &sea_orm::DatabaseTransaction,
    key_hash: &str,
) -> Result<Principal> {
    validate_digest(key_hash)?;
    let row = api_keys::Entity::find()
        .filter(api_keys::Column::KeyHash.eq(key_hash))
        .lock_exclusive()
        .one(tx)
        .await?
        .ok_or(Rejection::Unauthorized)?;
    ensure!(
        row.active == 1 && !crate::auth::db::is_reserved_user_id(&row.user_id),
        Rejection::Unauthorized
    );
    validate_id(&row.id)?;
    validate_id(&row.user_id)?;
    let now_ms = Store::now(tx).await?;
    let valid_until_ms = if let Some(expires) = &row.expires_at {
        let expiry =
            chrono::DateTime::parse_from_rfc3339(expires).map_err(|_| Rejection::Unauthorized)?;
        ensure!(expiry.timestamp_millis() > now_ms, Rejection::Unauthorized);
        Some(expiry.timestamp_millis())
    } else {
        None
    };
    // The local monthly-spend query is not an atomic shared budget reservation.
    // Reject an unsupported hard ceiling instead of silently weakening it.
    ensure!(row.spend_limit_micro_usd.is_none(), Rejection::Forbidden);
    let rpm = row
        .rpm_limit
        .map(u32::try_from)
        .transpose()
        .map_err(|_| Rejection::Forbidden)?;
    let revision = hex::encode(Sha256::digest(serde_json::to_vec(&(
        "gateway-authorization-v1",
        &row.id,
        &row.key_hash,
        &row.user_id,
        row.active,
        &row.expires_at,
        &row.policy_id,
        row.spend_limit_micro_usd,
        row.rpm_limit,
    ))?));
    Ok(Principal {
        id: row.id,
        tenant: row.user_id,
        revision,
        rpm,
        valid_until_ms,
    })
}

fn validate_digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid shared request digest"
    );
    Ok(())
}

fn idempotency_id(tenant: &str, key: &str) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
        "gateway-idempotency-v1",
        tenant,
        key,
    ))?)))
}
