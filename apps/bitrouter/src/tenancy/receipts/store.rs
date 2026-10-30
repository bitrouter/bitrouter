//! Bounded immutable bytes, written only by the existing authority actor.
use super::capacity::Inventory;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    execution::{ExecutionPage, ExecutionReceipt, ReceiptState},
    identity::{CorrelationId, ExecutionId, InstallationId, TenantId},
    operation::DispatchState,
    validation::Validate,
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseTransaction, QueryResult, SqlErr,
    sea_query::{Alias, Asterisk, Expr, OnConflict, Order, Query, SelectStatement},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(test)]
mod tests;

const TABLE: &str = "tenancy_execution_receipts";
const MAX_RECEIPT: usize = 1024 * 1024;
const MAX_DISPOSITION: usize = 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Disposition {
    pub dispatch: DispatchState,
    pub statuses: Vec<Option<u16>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auxiliary_digest: Option<String>,
}

pub(in crate::tenancy) struct Candidate {
    pub receipt: ExecutionReceipt,
    json: String,
    private_json: String,
    pub(in crate::tenancy) digest: String,
    pub(super) auxiliary: Option<super::auxiliary_store::Evidence>,
}

impl Candidate {
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) fn learning_digest(&self) -> &str {
        &self.digest
    }

    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) fn experiment_disposition(&self) -> Result<(&str, DispatchState)> {
        let disposition: Disposition = serde_json::from_str(&self.private_json)?;
        Ok((&self.digest, disposition.dispatch))
    }
    pub fn new(receipt: ExecutionReceipt, disposition: Disposition) -> Result<Self> {
        receipt.validate()?;
        ensure!(
            disposition.statuses.len() == receipt.attempts.len(),
            "receipt attempt disposition count"
        );
        ensure!(
            disposition
                .statuses
                .iter()
                .flatten()
                .all(|status| (200..=599).contains(status)),
            "receipt status range"
        );
        let json = serde_json::to_string(&receipt)?;
        let private_json = serde_json::to_string(&disposition)?;
        ensure!(
            json.len() <= MAX_RECEIPT && private_json.len() <= MAX_DISPOSITION,
            "receipt byte bound"
        );
        let digest = digest(&json, &private_json);
        Ok(Self {
            receipt,
            json,
            private_json,
            digest,
            auxiliary: None,
        })
    }
    #[cfg(all(test, unix))]
    pub(in crate::tenancy) fn fingerprint_for_test(
        &self,
    ) -> crate::tenancy::transaction::receipt_observation::Fingerprint {
        crate::tenancy::transaction::receipt_observation::Fingerprint {
            digest: self.digest.clone(),
            bytes: self.bytes(),
            attempt_count: self.receipt.attempts.len(),
            first_attempt: self
                .receipt
                .attempts
                .first()
                .map(|attempt| attempt.attempt_id.clone()),
        }
    }
    pub fn with_auxiliary(mut self, facts: impl Into<super::auxiliary::Snapshot>) -> Result<Self> {
        let facts = facts.into();
        if facts.is_empty() {
            return Ok(self);
        }
        let evidence = super::auxiliary_store::Evidence::new(&self.receipt, facts)?;
        let mut disposition: Disposition = serde_json::from_str(&self.private_json)?;
        disposition.auxiliary_digest = Some(evidence.digest.clone());
        self.private_json = serde_json::to_string(&disposition)?;
        ensure!(
            self.private_json.len() <= MAX_DISPOSITION,
            "receipt disposition byte bound"
        );
        self.digest = digest(&self.json, &self.private_json);
        self.auxiliary = Some(evidence);
        Ok(self)
    }
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) fn has_auxiliary(&self) -> bool {
        self.auxiliary.is_some()
    }
    pub fn auxiliary_unclean(&self) -> bool {
        self.auxiliary.as_ref().is_some_and(|aux| aux.unclean())
    }
    pub fn auxiliary_unknown(&self) -> bool {
        self.auxiliary.as_ref().is_some_and(|aux| aux.unknown())
    }
    pub fn auxiliary_bytes(&self) -> u64 {
        self.auxiliary
            .as_ref()
            .map_or(0, |aux| aux.json.len() as u64 + 16 * 1024)
    }
    pub fn bytes(&self) -> u64 {
        (self.json.len() + self.private_json.len()) as u64
    }
    pub fn unknown(&self) -> bool {
        self.receipt.state == ReceiptState::Unknown
    }
}

fn digest(json: &str, disposition: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"bitrouter.receipt.immutable.v1");
    for part in [json, disposition] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hex::encode(hash.finalize())
}
fn scope(installation: &InstallationId, tenant: &TenantId) -> SelectStatement {
    Query::select()
        .from(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(tenant.as_str()))
        .to_owned()
}
fn point(installation: &InstallationId, tenant: &TenantId, id: &ExecutionId) -> SelectStatement {
    scope(installation, tenant)
        .and_where(Expr::col(Alias::new("execution_id")).eq(id.as_str()))
        .to_owned()
}

fn octets(backend: DatabaseBackend, name: &str) -> String {
    if backend == DatabaseBackend::Sqlite {
        format!("length(CAST({name} AS BLOB))")
    } else {
        format!("octet_length({name})")
    }
}
fn oversized(backend: DatabaseBackend) -> String {
    [
        ("receipt_json", MAX_RECEIPT),
        ("disposition_json", MAX_DISPOSITION),
        ("installation_id", 128),
        ("tenant_id", 128),
        ("execution_id", 128),
        ("principal_id", 128),
        ("correlation_id", 128),
        ("content_digest", 64),
        ("recorded_at", 20),
    ]
    .into_iter()
    .map(|(name, max)| format!("{} > {max}", octets(backend, name)))
    .collect::<Vec<_>>()
    .join(" OR ")
}
async fn row(db: &impl ConnectionTrait, query: SelectStatement) -> Result<Option<QueryResult>> {
    bounded_row(db, query, None).await
}
async fn bounded_row(
    db: &impl ConnectionTrait,
    mut query: SelectStatement,
    budget: Option<&crate::storage::operation_budget::OperationBudget<'_>>,
) -> Result<Option<QueryResult>> {
    let backend = db.get_database_backend();
    // Bound stored TEXT before transferring it; a corrupt cell is never materialized.
    let length = oversized(backend);
    let oversized = query
        .clone()
        .expr(Expr::val(1))
        .and_where(Expr::cust(length))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&oversized)).await?.is_none(),
        "stored receipt byte bound"
    );
    if let Some(budget) = budget {
        budget.check()?;
        let lengths = [
            "receipt_json",
            "disposition_json",
            "installation_id",
            "tenant_id",
            "execution_id",
            "principal_id",
            "correlation_id",
            "content_digest",
            "recorded_at",
        ]
        .into_iter()
        .map(|name| octets(backend, name))
        .collect::<Vec<_>>()
        .join(" + ");
        let cast = if backend == DatabaseBackend::MySql {
            "SIGNED"
        } else {
            "BIGINT"
        };
        let mut size = query.clone();
        size.expr_as(
            Expr::cust(format!("CAST(({lengths} + 256) AS {cast})")),
            Alias::new("receipt_bytes"),
        )
        .limit(1);
        budget.charge(1, 256)?;
        let Some(metadata) = db.query_one(backend.build(&size)).await? else {
            return Ok(None);
        };
        let bytes = usize::try_from(metadata.try_get::<i64>("", "receipt_bytes")?)?;
        budget.charge(1, bytes)?;
    }
    query.column(Asterisk).limit(1);
    let value = db.query_one(backend.build(&query)).await?;
    if let Some(budget) = budget {
        budget.check()?;
    }
    Ok(value)
}

fn decode(
    row: QueryResult,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<Candidate> {
    let json: String = row.try_get("", "receipt_json")?;
    let private: String = row.try_get("", "disposition_json")?;
    ensure!(
        json.len() <= MAX_RECEIPT && private.len() <= MAX_DISPOSITION,
        "receipt byte bound"
    );
    let candidate = Candidate::new(
        serde_json::from_str(&json)?,
        serde_json::from_str(&private)?,
    )?;
    let receipt = &candidate.receipt;
    ensure!(
        &receipt.installation_id == installation
            && &receipt.tenant_id == tenant
            && &receipt.execution_id == id,
        "receipt namespace mismatch"
    );
    for (column, value) in [
        ("installation_id", installation.as_str()),
        ("tenant_id", tenant.as_str()),
        ("execution_id", id.as_str()),
        ("principal_id", receipt.principal_id.as_str()),
        ("correlation_id", receipt.correlation_id.as_str()),
        ("recorded_at", receipt.recorded_at.as_str()),
    ] {
        ensure!(
            row.try_get::<String>("", column)? == value,
            "receipt projection mismatch"
        );
    }
    ensure!(
        candidate.json == json
            && candidate.private_json == private
            && row.try_get::<String>("", "content_digest")? == candidate.digest
            && row.try_get::<i64>("", "byte_count")? == i64::try_from(candidate.bytes())?
            && row.try_get::<bool>("", "unknown_outcome")? == candidate.unknown(),
        "receipt integrity mismatch"
    );
    Ok(candidate)
}

pub(in crate::tenancy) async fn lookup(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<Option<ExecutionReceipt>> {
    Ok(lookup_candidate(db, installation, tenant, id)
        .await?
        .map(|candidate| candidate.receipt))
}

/// Inventory and pending-work reconciliation use the same bounded integrity
/// checks as a point read; stored digest projections alone are not evidence.
pub(in crate::tenancy) async fn lookup_candidate(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<Option<Candidate>> {
    let Some(row) = row(db, point(installation, tenant, id)).await? else {
        return Ok(None);
    };
    let mut candidate = decode(row, installation, tenant, id)?;
    let disposition: Disposition = serde_json::from_str(&candidate.private_json)?;
    super::auxiliary_store::validate(db, &mut candidate, disposition.auxiliary_digest.as_deref())
        .await?;
    Ok(Some(candidate))
}

/// The actor owns this transaction through confirmed commit/rollback. Reconciliation
/// compares exact immutable bytes; it never replays inference or widens authority.
pub(in crate::tenancy) async fn insert(
    tx: &DatabaseTransaction,
    candidate: &Candidate,
) -> Result<bool> {
    let receipt = &candidate.receipt;
    let existing = row(
        tx,
        point(
            &receipt.installation_id,
            &receipt.tenant_id,
            &receipt.execution_id,
        ),
    )
    .await?;
    if let Some(row) = existing {
        let saved = decode(
            row,
            &receipt.installation_id,
            &receipt.tenant_id,
            &receipt.execution_id,
        )?;
        ensure!(
            saved.digest == candidate.digest
                && saved.json == candidate.json
                && saved.private_json == candidate.private_json,
            "immutable receipt conflict"
        );
        return Ok(false);
    }
    let fields: Vec<(&str, sea_orm::Value)> = vec![
        ("installation_id", receipt.installation_id.as_str().into()),
        ("tenant_id", receipt.tenant_id.as_str().into()),
        ("execution_id", receipt.execution_id.as_str().into()),
        ("principal_id", receipt.principal_id.as_str().into()),
        ("correlation_id", receipt.correlation_id.as_str().into()),
        ("receipt_json", candidate.json.clone().into()),
        ("content_digest", candidate.digest.clone().into()),
        ("disposition_json", candidate.private_json.clone().into()),
        ("byte_count", i64::try_from(candidate.bytes())?.into()),
        ("unknown_outcome", candidate.unknown().into()),
        ("recorded_at", receipt.recorded_at.as_str().into()),
    ];
    let mut query = Query::insert();
    query
        .into_table(Alias::new(TABLE))
        .columns(fields.iter().map(|(name, _)| Alias::new(*name)))
        .values(fields.into_iter().map(|(_, value)| value.into()))?;
    let backend = tx.get_database_backend();
    if backend != DatabaseBackend::MySql {
        query.on_conflict(OnConflict::new().do_nothing().to_owned());
    }
    let changed = match tx.execute(backend.build(&query)).await {
        Ok(result) => {
            ensure!(result.rows_affected() <= 1, "receipt insert count");
            result.rows_affected() == 1
        }
        Err(error)
            if backend == DatabaseBackend::MySql
                && matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) =>
        {
            false
        }
        Err(error) => return Err(error.into()),
    };
    let saved = decode(
        row(
            tx,
            point(
                &receipt.installation_id,
                &receipt.tenant_id,
                &receipt.execution_id,
            ),
        )
        .await?
        .context("receipt physical collision")?,
        &receipt.installation_id,
        &receipt.tenant_id,
        &receipt.execution_id,
    )?;
    ensure!(
        saved.digest == candidate.digest
            && saved.json == candidate.json
            && saved.private_json == candidate.private_json,
        "immutable receipt conflict"
    );
    Ok(changed)
}

/// Caller supplies one stable read transaction and rechecks current permission
/// after it. Multiplicity is query-wide, independent of cursor and page size.
pub(in crate::tenancy) async fn page(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    correlation: &CorrelationId,
    after: Option<&ExecutionId>,
    limit: u64,
) -> Result<ExecutionPage> {
    ensure!((1..=100).contains(&limit), "receipt page limit");
    let backend = db.get_database_backend();
    let invalid = scope(installation, tenant)
        .expr(Expr::val(1))
        .and_where(Expr::col(Alias::new("correlation_id")).eq(correlation.as_str()))
        .and_where(Expr::cust(format!(
            "{} > 128",
            octets(backend, "execution_id")
        )))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&invalid)).await?.is_none(),
        "receipt page identity byte bound"
    );
    let mut query = scope(installation, tenant);
    query
        .and_where(Expr::col(Alias::new("correlation_id")).eq(correlation.as_str()))
        .column(Alias::new("execution_id"))
        .order_by(Alias::new("execution_id"), Order::Asc)
        .limit(2);
    let multiple_executions = db.query_all(backend.build(&query)).await?.len() > 1;
    if let Some(after) = after {
        query.and_where(Expr::col(Alias::new("execution_id")).gt(after.as_str()));
    }
    query.limit(limit + 1);
    let rows = db.query_all(backend.build(&query)).await?;
    let more = rows.len() > usize::try_from(limit)?;
    let execution_ids = rows
        .into_iter()
        .take(usize::try_from(limit)?)
        .map(|row| {
            let raw: String = row.try_get("", "execution_id")?;
            Ok(raw.try_into()?)
        })
        .collect::<Result<Vec<ExecutionId>>>()?;
    let next_after = if more {
        execution_ids.last().cloned()
    } else {
        None
    };
    let page = ExecutionPage {
        execution_ids,
        multiple_executions,
        next_after,
        installation_id: installation.clone(),
        correlation_id: correlation.clone(),
    };
    page.validate()?;
    Ok(page)
}

pub(in crate::tenancy) async fn inventory(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
) -> Result<Inventory> {
    let backend = db.get_database_backend();
    let query = Query::select()
        .from(Alias::new(TABLE))
        .expr(Expr::val(1))
        .and_where(Expr::col(Alias::new("installation_id")).ne(installation.as_str()))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&query)).await?.is_none(),
        "foreign receipt installation"
    );
    // Do not trust a stored count that can understate the reserved payload. The
    // persistent schema bounds identity/index bytes; measure actual TEXT octets
    // before SUM. No payload is transferred during this check.
    let exact = format!(
        "byte_count != {} + {} OR byte_count < 1 OR {}",
        octets(backend, "receipt_json"),
        octets(backend, "disposition_json"),
        oversized(backend)
    );
    let invalid = Query::select()
        .from(Alias::new(TABLE))
        .expr(Expr::val(1))
        .and_where(Expr::cust(exact))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&invalid)).await?.is_none(),
        "receipt inventory byte count"
    );
    let unknown_mismatch = match backend {
        DatabaseBackend::Sqlite => {
            "json_extract(receipt_json, '$.state') IS NULL OR json_extract(receipt_json, '$.state') NOT IN ('committed','incomplete','unknown') OR unknown_outcome != (json_extract(receipt_json, '$.state') = 'unknown')"
        }
        DatabaseBackend::Postgres => {
            "receipt_json::jsonb->>'state' IS NULL OR receipt_json::jsonb->>'state' NOT IN ('committed','incomplete','unknown') OR unknown_outcome <> (receipt_json::jsonb->>'state' = 'unknown')"
        }
        DatabaseBackend::MySql => {
            "JSON_EXTRACT(receipt_json, '$.state') IS NULL OR JSON_UNQUOTE(JSON_EXTRACT(receipt_json, '$.state')) NOT IN ('committed','incomplete','unknown') OR unknown_outcome <> (JSON_UNQUOTE(JSON_EXTRACT(receipt_json, '$.state')) = 'unknown')"
        }
    };
    let invalid = Query::select()
        .from(Alias::new(TABLE))
        .expr(Expr::val(1))
        .and_where(Expr::cust(unknown_mismatch))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&invalid)).await?.is_none(),
        "receipt unknown projection mismatch"
    );
    let query = Query::select()
        .from(Alias::new(TABLE))
        .expr_as(
            Expr::cust(if backend == DatabaseBackend::MySql {
                "CAST(SUM(byte_count) AS SIGNED)"
            } else {
                "CAST(SUM(byte_count) AS BIGINT)"
            }),
            Alias::new("bytes"),
        )
        .expr_as(
            Expr::cust(if backend == DatabaseBackend::MySql {
                "CAST(COUNT(*) AS SIGNED)"
            } else {
                "CAST(COUNT(*) AS BIGINT)"
            }),
            Alias::new("rows"),
        )
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .to_owned();
    let row = db
        .query_one(backend.build(&query))
        .await?
        .context("receipt inventory")?;
    let bytes = row.try_get::<Option<i64>>("", "bytes")?.unwrap_or(0);
    let mut count = u64::try_from(row.try_get::<i64>("", "rows")?)?;
    let query = Query::select()
        .from(Alias::new(TABLE))
        .columns(["tenant_id", "execution_id", "recorded_at"].map(Alias::new))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .and_where(Expr::col(Alias::new("unknown_outcome")).eq(true))
        .limit(1001)
        .to_owned();
    let rows = db.query_all(backend.build(&query)).await?;
    ensure!(rows.len() <= 1000, "receipt unknown instance bound");
    let mut unknown = Vec::with_capacity(rows.len());
    let mut counts = std::collections::BTreeMap::new();
    for row in rows {
        let tenant: TenantId = row.try_get::<String>("", "tenant_id")?.try_into()?;
        let id: ExecutionId = row.try_get::<String>("", "execution_id")?.try_into()?;
        let recorded =
            chrono::DateTime::parse_from_rfc3339(&row.try_get::<String>("", "recorded_at")?)?
                .with_timezone(&chrono::Utc);
        let count = counts.entry(tenant.clone()).or_insert(0_usize);
        *count += 1;
        ensure!(*count <= 100, "receipt unknown tenant bound");
        unknown.push((tenant, id, recorded));
    }
    let (aux_bytes, aux_unknown) = super::auxiliary_store::inventory(db, installation).await?;
    for (tenant, id, recorded) in aux_unknown {
        if unknown.iter().any(|(t, e, _)| t == &tenant && e == &id) {
            continue;
        }
        let count = counts.entry(tenant.clone()).or_default();
        *count += 1;
        ensure!(
            *count <= 100 && unknown.len() < 1000,
            "auxiliary unknown capacity"
        );
        unknown.push((tenant, id, recorded));
    }
    for (tenant, id, recorded) in
        crate::tenancy::oauth::storage::pending_unknown(db, installation).await?
    {
        if unknown.iter().any(|(t, e, _)| t == &tenant && e == &id) {
            continue;
        }
        let tenant_count = counts.entry(tenant.clone()).or_default();
        *tenant_count += 1;
        ensure!(
            *tenant_count <= 100 && unknown.len() < 1000,
            "pending OAuth unknown capacity"
        );
        // Private accounting rows include this persisted obligation, not a
        // fabricated public receipt or model attempt. Slot bytes are separately
        // charged at their worst-case envelope, so this overhead is conservative.
        count = count
            .checked_add(1)
            .context("pending OAuth accounting overflow")?;
        unknown.push((tenant, id, recorded));
    }
    let oauth_bytes = crate::tenancy::oauth::storage::inventory(db, installation).await?;
    Ok(Inventory {
        bytes: u64::try_from(bytes)?
            .checked_add(aux_bytes)
            .and_then(|bytes| bytes.checked_add(oauth_bytes))
            .context("auxiliary receipt inventory overflow")?,
        rows: count,
        unknown,
    })
}

#[cfg(all(feature = "core", feature = "harness"))]
pub(in crate::tenancy) async fn recovery_lookup(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
    budget: &crate::tenancy::session::recovery::Budget,
) -> Result<Option<Candidate>> {
    ensure!(
        db.get_database_backend() == DatabaseBackend::Sqlite,
        "native recovery requires SQLite"
    );
    let mut invalid = point(installation, tenant, id);
    invalid.expr(Expr::val(1)).and_where(Expr::cust(
        "typeof(byte_count)!='integer' OR typeof(unknown_outcome)!='integer' OR unknown_outcome NOT IN (0,1) OR typeof(receipt_json)!='text' OR typeof(disposition_json)!='text'"
    )).limit(1);
    ensure!(
        db.query_one(db.get_database_backend().build(&invalid))
            .await?
            .is_none(),
        "invalid receipt recovery SQL types"
    );
    budget.charge(0, 1024)?;
    let mut metadata = point(installation, tenant, id);
    metadata
        .expr_as(
            Expr::cust("CASE WHEN typeof(byte_count)='integer' THEN byte_count ELSE NULL END"),
            Alias::new("byte_count"),
        )
        .expr_as(
            Expr::cust(format!(
                "CAST({} + {} AS BIGINT)",
                octets(db.get_database_backend(), "receipt_json"),
                octets(db.get_database_backend(), "disposition_json")
            )),
            Alias::new("actual"),
        )
        .limit(1);
    let metadata_row = db
        .query_one(db.get_database_backend().build(&metadata))
        .await?;
    if let Some(row) = metadata_row {
        let bytes: i64 = row.try_get("", "byte_count")?;
        let actual: i64 = row.try_get("", "actual")?;
        ensure!(
            bytes == actual && bytes >= 0 && bytes as u64 <= (MAX_RECEIPT + MAX_DISPOSITION) as u64,
            "receipt recovery length"
        );
        budget.charge(
            1,
            usize::try_from(bytes)?
                .checked_mul(3)
                .context("receipt recovery allocation bound")?,
        )?;
    } else {
        budget.charge(1, 0)?;
    }
    // This first profile has no auxiliary recovery proof. Check presence without
    // materializing O1/C1 evidence outside the shared operation byte budget.
    let auxiliary = Query::select()
        .expr(Expr::val(1))
        .from(Alias::new("tenancy_receipt_auxiliary"))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(tenant.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(id.as_str()))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(db.get_database_backend().build(&auxiliary))
            .await?
            .is_none(),
        "auxiliary receipt work has no stopped recovery proof"
    );
    let Some(row) = row(db, point(installation, tenant, id)).await? else {
        return Ok(None);
    };
    let candidate = decode(row, installation, tenant, id)?;
    let disposition: Disposition = serde_json::from_str(&candidate.private_json)?;
    ensure!(
        disposition.auxiliary_digest.is_none(),
        "auxiliary receipt link has no recovery proof"
    );
    Ok(Some(candidate))
}

#[cfg(all(feature = "core", feature = "harness"))]
pub(in crate::tenancy) fn reserve_experiment_write(
    budget: &crate::storage::operation_budget::OperationBudget<'_>,
) -> Result<()> {
    // The existing insert performs at most two bounded point loads and one
    // immutable write. Each decode may validate one bounded auxiliary row;
    // auxiliary insertion additionally writes and compares that same body.
    // Reserve the full transfer envelope before the first I/O, so the original
    // receipt and experiment participant cannot each claim a fresh allowance.
    let receipt = MAX_RECEIPT + MAX_DISPOSITION + 4096;
    let auxiliary = super::auxiliary::MAX_BYTES + 4096;
    budget.charge(16, 3 * receipt + 4 * auxiliary)?;
    Ok(())
}

#[cfg(all(feature = "core", feature = "harness"))]
pub(in crate::tenancy) async fn native_candidate_in(
    tx: &DatabaseTransaction,
    authority: &crate::tenancy::execution::session::Authority,
    id: &ExecutionId,
    budget: &crate::storage::operation_budget::OperationBudget<'_>,
) -> Result<Option<Candidate>> {
    let (installation, tenant, _) = authority.storage_identity();
    native_namespace_candidate_in(tx, installation, tenant, id, budget).await
}

#[cfg(all(feature = "core", feature = "harness"))]
pub(in crate::tenancy) async fn learner_candidate_in(
    tx: &sea_orm::DatabaseTransaction,
    identity: &crate::tenancy::learner::identity::ReadIdentity,
    id: &ExecutionId,
    budget: &crate::storage::operation_budget::OperationBudget<'_>,
) -> Result<Option<Candidate>> {
    identity
        .check(&[])
        .map_err(crate::storage::operation_budget::Error::Rejected)?;
    let (installation, tenant, _) = identity.storage_identity();
    native_namespace_candidate_in(tx, installation, tenant, id, budget).await
}

#[cfg(all(feature = "core", feature = "harness"))]
pub(in crate::tenancy) async fn paid_recovery_candidate_in(
    tx: &DatabaseTransaction,
    identity: &crate::tenancy::learning::recovery::RecoveryIdentity,
    id: &ExecutionId,
    budget: &crate::storage::operation_budget::OperationBudget<'_>,
) -> Result<Option<Candidate>> {
    let (installation, tenant, _) = identity.storage_identity();
    native_namespace_candidate_in(tx, installation, tenant, id, budget).await
}

/// The live original capture authority supplies the namespace; an imported
/// descriptor or an opaque-link string can never invoke this reader by itself.
#[cfg(all(feature = "core", feature = "harness"))]
pub(in crate::tenancy) async fn block_candidate_in(
    tx: &DatabaseTransaction,
    binding: &crate::tenancy::capture::Binding,
    id: &ExecutionId,
    budget: &crate::storage::operation_budget::OperationBudget<'_>,
) -> Result<Option<Candidate>> {
    binding
        .check()
        .map_err(|failure| crate::storage::operation_budget::Error::Rejected(failure.0.code))?;
    let (installation, tenant, _) = binding.authority().identity();
    native_namespace_candidate_in(tx, installation, tenant, id, budget).await
}

#[cfg(all(feature = "core", feature = "harness"))]
async fn native_namespace_candidate_in(
    tx: &sea_orm::DatabaseTransaction,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
    budget: &crate::storage::operation_budget::OperationBudget<'_>,
) -> Result<Option<Candidate>> {
    let Some(row) = bounded_row(tx, point(installation, tenant, id), Some(budget)).await? else {
        return Ok(None);
    };
    let mut candidate = decode(row, installation, tenant, id)?;
    // The auxiliary query preflights its fixed-size cells. Reserve its full
    // allowance on this same budget before issuing it; static experiment routes
    // are expected to have no auxiliary body, but corruption is still checked.
    budget.charge(1, super::auxiliary::MAX_BYTES + 4096)?;
    let disposition: Disposition = serde_json::from_str(&candidate.private_json)?;
    super::auxiliary_store::validate(tx, &mut candidate, disposition.auxiliary_digest.as_deref())
        .await?;
    budget.check()?;
    Ok(Some(candidate))
}
