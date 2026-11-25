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
    sea_query::{Alias, Expr, OnConflict, Order, Query, SelectStatement},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(in crate::tenancy) mod archive;
#[cfg(test)]
mod capacity_tests;
pub(super) mod inventory;
mod load;
pub(crate) mod obligations;
#[cfg(test)]
mod terminal_tests;
#[cfg(test)]
mod tests;

pub(in crate::tenancy) mod format;

const TABLE: &str = "tenancy_execution_receipts";
pub(super) const MAX_RECEIPT: usize = 1024 * 1024;
pub(super) const MAX_DISPOSITION: usize = 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Disposition {
    pub dispatch: DispatchState,
    pub statuses: Vec<Option<u16>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auxiliary_digest: Option<String>,
}

// The durable disposition binds the receipt to its required usage evidence.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredDisposition {
    pub(super) dispatch: DispatchState,
    pub(super) statuses: Vec<Option<u16>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) auxiliary_digest: Option<String>,
    pub(super) usage_evidence_digest: String,
}
impl StoredDisposition {
    pub(super) fn new(value: Disposition, usage_evidence_digest: String) -> Self {
        Self {
            dispatch: value.dispatch,
            statuses: value.statuses,
            auxiliary_digest: value.auxiliary_digest,
            usage_evidence_digest,
        }
    }
}

pub(in crate::tenancy) struct Candidate {
    pub receipt: ExecutionReceipt,
    json: String,
    private_json: String,
    pub(in crate::tenancy) digest: String,
    pub(super) auxiliary: Option<super::auxiliary_store::Evidence>,
    pub(super) usage_evidence: super::usage_evidence::storage::Evidence,
}

impl Candidate {
    #[cfg(test)]
    pub fn new(receipt: ExecutionReceipt, disposition: Disposition) -> Result<Self> {
        Self::captured(
            receipt,
            disposition,
            super::usage_evidence::Snapshot::default(),
        )
    }

    pub(super) fn captured(
        receipt: ExecutionReceipt,
        disposition: Disposition,
        snapshot: super::usage_evidence::Snapshot,
    ) -> Result<Self> {
        let document = snapshot.document(&receipt);
        let usage_evidence = super::usage_evidence::storage::Evidence::new(document, &receipt)?;
        let disposition = StoredDisposition::new(disposition, usage_evidence.digest.clone());
        let (json, private_json) = encode_strict(&receipt, &disposition)?;
        let digest = digest(&json, &private_json);
        Ok(Self {
            receipt,
            json,
            private_json,
            digest,
            auxiliary: None,
            usage_evidence,
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
        let mut disposition: StoredDisposition = serde_json::from_str(&self.private_json)?;
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
    #[cfg(test)]
    pub fn with_usage_evidence(self, snapshot: super::usage_evidence::Snapshot) -> Result<Self> {
        let document = snapshot.document(&self.receipt);
        self.with_usage_document(document)
    }
    /// Fault fixtures may change another receipt projection while retaining the
    /// exact original usage document and rebinding its immutable receipt digest.
    #[cfg(test)]
    pub(in crate::tenancy) fn with_preserved_usage_evidence_for_test(
        self,
        original: &Self,
    ) -> Result<Self> {
        self.with_usage_document(original.usage_evidence.document.clone())
    }
    #[cfg(test)]
    fn with_usage_document(
        mut self,
        document: bitrouter_ai::tenancy::usage_evidence::ExecutionUsageEvidence,
    ) -> Result<Self> {
        let evidence = super::usage_evidence::storage::Evidence::new(document, &self.receipt)?;
        let mut disposition: StoredDisposition = serde_json::from_str(&self.private_json)?;
        disposition.usage_evidence_digest = evidence.digest.clone();
        self.private_json = serde_json::to_string(&disposition)?;
        ensure!(
            self.private_json.len() <= MAX_DISPOSITION,
            "receipt disposition byte bound"
        );
        self.digest = digest(&self.json, &self.private_json);
        self.usage_evidence = evidence;
        Ok(self)
    }
    pub fn auxiliary_unclean(&self) -> bool {
        self.auxiliary.as_ref().is_some_and(|aux| aux.unclean())
    }
    pub fn auxiliary_unknown(&self) -> bool {
        self.auxiliary.as_ref().is_some_and(|aux| aux.unknown())
    }
    pub fn auxiliary_bytes(&self) -> u64 {
        let usage = self.usage_evidence.json.len() as u64 + 16 * 1024;
        usage
            + self
                .auxiliary
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

fn encode_strict(
    receipt: &ExecutionReceipt,
    disposition: &StoredDisposition,
) -> Result<(String, String)> {
    receipt.validate()?;
    ensure!(
        matches!(
            receipt.dispatch,
            bitrouter_ai::tenancy::execution::Dispatch::Strict
        ),
        "strict storage dispatch"
    );
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
    let json = serde_json::to_string(receipt)?;
    let private_json = serde_json::to_string(disposition)?;
    ensure!(
        json.len() <= MAX_RECEIPT && private_json.len() <= MAX_DISPOSITION,
        "receipt byte bound"
    );
    Ok((json, private_json))
}

fn digest(json: &str, disposition: &str) -> String {
    digest_for(format::Format::Strict, json, disposition)
}
pub(super) fn digest_for(format: format::Format, json: &str, disposition: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(match format {
        format::Format::Strict => b"bitrouter.receipt.immutable.v1",
        format::Format::Ordered => b"bitrouter.receipt.immutable.v2",
    });
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
    budget: Option<&archive::Charge<'_>>,
) -> Result<Option<QueryResult>> {
    let backend = db.get_database_backend();
    let format = if backend == DatabaseBackend::Sqlite {
        "CASE WHEN typeof(receipt_format)='integer' THEN receipt_format ELSE NULL END"
    } else {
        "receipt_format"
    };
    query.expr_as(Expr::cust(format), Alias::new("receipt_format"));

    // Callers supply an exact receipt key. Charged SQLite reads combine both
    // scalar probes; other readers retain the original oversized-row probe.
    let combined = backend == DatabaseBackend::Sqlite && budget.is_some();
    if !combined {
        let oversized = query
            .clone()
            .clear_selects()
            .expr(Expr::val(1))
            .and_where(Expr::cust(oversized(backend)))
            .limit(1)
            .to_owned();
        ensure!(
            db.query_one(backend.build(&oversized)).await?.is_none(),
            "stored receipt byte bound"
        );
    }
    let mut transfer_guard = String::new();
    if let Some(budget) = budget {
        budget(0, 0)?;
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
        let measured = format!("CAST(({lengths} + 256) AS {cast})");
        let measured = if combined {
            // NULL marks an invalid present row; never filter it into absence.
            // Reserve before transfer even when cancellation masks that error.
            format!(
                "CASE WHEN ({}) THEN NULL ELSE {measured} END",
                oversized(backend)
            )
        } else {
            measured
        };
        let mut size = query.clone();
        size.expr_as(Expr::cust(measured), Alias::new("receipt_bytes"))
            .limit(1);
        budget(1, 256)?;
        let Some(metadata) = db.query_one(backend.build(&size)).await? else {
            return Ok(None);
        };
        let bytes = usize::try_from(metadata.try_get::<i64>("", "receipt_bytes")?)?;
        budget(
            1,
            bytes.checked_mul(3).context("receipt decode allocation")?,
        )?;
        transfer_guard = format!(" AND ({lengths} + 256)={bytes}");
    }
    // Reapply guards in the transfer query; a changed/corrupt cell never crosses
    // the driver boundary merely because a previous preflight was valid.
    for (name, bound) in [
        ("receipt_json", MAX_RECEIPT),
        ("disposition_json", MAX_DISPOSITION),
        ("installation_id", 128),
        ("tenant_id", 128),
        ("execution_id", 128),
        ("principal_id", 128),
        ("correlation_id", 128),
        ("content_digest", 64),
        ("recorded_at", 20),
    ] {
        let kind = if backend == DatabaseBackend::Sqlite {
            format!("typeof({name})='text' AND ")
        } else {
            String::new()
        };
        query.expr_as(
            Expr::cust(format!(
                "CASE WHEN {kind}{} <= {bound}{transfer_guard} THEN {name} ELSE NULL END",
                octets(backend, name)
            )),
            Alias::new(name),
        );
    }
    for name in ["byte_count", "unknown_outcome"] {
        let cell = if backend == DatabaseBackend::Sqlite {
            let valid = if name == "unknown_outcome" {
                " AND unknown_outcome IN (0,1)"
            } else {
                ""
            };
            format!("CASE WHEN typeof({name})='integer'{valid} THEN {name} ELSE NULL END")
        } else {
            name.to_owned()
        };
        query.expr_as(Expr::cust(cell), Alias::new(name));
    }
    query
        .expr_as(
            Expr::cust(match backend {
                DatabaseBackend::Sqlite => {
                    "CASE WHEN typeof(storage_kind)='integer' THEN storage_kind ELSE NULL END"
                }
                DatabaseBackend::Postgres => "CAST(storage_kind AS BIGINT)",
                DatabaseBackend::MySql => "CAST(0 AS SIGNED)",
            }),
            Alias::new("storage_kind"),
        )
        .limit(1);
    let value = db.query_one(backend.build(&query)).await?;
    if let Some(budget) = budget {
        budget(0, 0)?;
    }
    Ok(value)
}

fn decode(
    row: &QueryResult,
    json: &str,
    private: &str,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<(ExecutionReceipt, StoredDisposition, String)> {
    ensure!(
        json.len() <= MAX_RECEIPT && private.len() <= MAX_DISPOSITION,
        "receipt byte bound"
    );
    ensure!(
        row.try_get::<i32>("", "receipt_format")? == 1,
        "receipt format mismatch"
    );
    let receipt: ExecutionReceipt = serde_json::from_str(json)?;
    let disposition: StoredDisposition = serde_json::from_str(private)?;
    let (encoded, encoded_private) = encode_strict(&receipt, &disposition)?;
    let digest = digest(json, private);
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
        encoded == json
            && encoded_private == private
            && row.try_get::<String>("", "content_digest")? == digest
            && row.try_get::<i64>("", "byte_count")? == i64::try_from(json.len() + private.len())?
            && row.try_get::<bool>("", "unknown_outcome")?
                == (receipt.state == ReceiptState::Unknown),
        "receipt integrity mismatch"
    );
    Ok((receipt, disposition, digest))
}

#[cfg(test)]
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
    load::candidate(db, installation, tenant, id, None, false).await
}

/// Shared bounded read for maintenance and recovery, including archived bodies.
/// The charge callback must belong to the caller's existing operation budget.
pub(in crate::tenancy) async fn lookup_candidate_bounded(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
    charge: &archive::Charge<'_>,
) -> Result<Option<Candidate>> {
    load::candidate(db, installation, tenant, id, Some(charge), false).await
}

fn same_completion(saved: &Candidate, candidate: &Candidate) -> Result<()> {
    ensure!(
        saved.digest == candidate.digest
            && saved.json == candidate.json
            && saved.private_json == candidate.private_json
            && saved.usage_evidence.json == candidate.usage_evidence.json
            && saved.usage_evidence.digest == candidate.usage_evidence.digest
            && saved.auxiliary.as_ref().map(|e| (&e.json, &e.digest))
                == candidate.auxiliary.as_ref().map(|e| (&e.json, &e.digest)),
        "immutable receipt conflict"
    );
    Ok(())
}

async fn insert_row(tx: &DatabaseTransaction, candidate: &Candidate) -> Result<bool> {
    let receipt = &candidate.receipt;
    if let Some(saved) = lookup_candidate(
        tx,
        &receipt.installation_id,
        &receipt.tenant_id,
        &receipt.execution_id,
    )
    .await?
    {
        same_completion(&saved, candidate)?;
        return Ok(false);
    }
    insert_new_row(tx, candidate).await
}

async fn insert_new_row(tx: &DatabaseTransaction, candidate: &Candidate) -> Result<bool> {
    let receipt = &candidate.receipt;
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
    if !changed {
        let saved = lookup_candidate(
            tx,
            &receipt.installation_id,
            &receipt.tenant_id,
            &receipt.execution_id,
        )
        .await?
        .context("receipt physical collision")?;
        same_completion(&saved, candidate)?;
        return Ok(false);
    }
    crate::db::migration::capacity::adjust(
        tx,
        crate::db::migration::capacity::Delta {
            live_bytes: i64::try_from(
                candidate
                    .bytes()
                    .checked_add(super::capacity::ROW_OVERHEAD)
                    .context("receipt capacity overflow")?,
            )?,
            ..Default::default()
        },
    )
    .await?;
    Ok(true)
}

/// The actor owns this transaction through confirmed commit/rollback. Reconciliation
/// compares exact immutable bytes; it never replays inference or widens authority.
pub(in crate::tenancy) async fn insert(
    tx: &DatabaseTransaction,
    candidate: &Candidate,
) -> Result<bool> {
    let changed = insert_row(tx, candidate).await?;
    if !changed {
        return Ok(false);
    }
    let receipt = &candidate.receipt;
    let row = row(
        tx,
        point(
            &receipt.installation_id,
            &receipt.tenant_id,
            &receipt.execution_id,
        ),
    )
    .await?
    .context("receipt physical collision")?;
    let json: String = row.try_get("", "receipt_json")?;
    let private: String = row.try_get("", "disposition_json")?;
    let (_, _, digest) = decode(
        &row,
        &json,
        &private,
        &receipt.installation_id,
        &receipt.tenant_id,
        &receipt.execution_id,
    )?;
    ensure!(
        digest == candidate.digest && json == candidate.json && private == candidate.private_json,
        "immutable receipt conflict"
    );
    super::usage_evidence::storage::insert(tx, candidate, changed).await?;
    Ok(changed)
}

struct Row<'a> {
    facts: format::Facts<'a>,
    format: format::Format,
    json: &'a str,
    private: &'a str,
    digest: &'a str,
}
async fn insert_bytes(tx: &DatabaseTransaction, value: Row<'_>) -> Result<bool> {
    let r = &value.facts;
    let fields: Vec<(&str, sea_orm::Value)> = vec![
        ("installation_id", r.installation.as_str().into()),
        ("tenant_id", r.tenant.as_str().into()),
        ("execution_id", r.execution.as_str().into()),
        ("principal_id", r.principal.as_str().into()),
        ("correlation_id", r.correlation.as_str().into()),
        ("receipt_format", value.format.number().into()),
        ("receipt_json", value.json.into()),
        ("content_digest", value.digest.into()),
        ("disposition_json", value.private.into()),
        (
            "byte_count",
            i64::try_from(value.json.len() + value.private.len())?.into(),
        ),
        ("unknown_outcome", (r.state == ReceiptState::Unknown).into()),
        ("recorded_at", r.recorded_at.as_str().into()),
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
    let saved = row(tx, point(r.installation, r.tenant, r.execution))
        .await?
        .context("receipt physical collision")?;
    ensure!(
        saved.try_get::<String>("", "receipt_json")? == value.json
            && saved.try_get::<String>("", "disposition_json")? == value.private
            && saved.try_get::<String>("", "content_digest")? == value.digest
            && saved.try_get::<i32>("", "receipt_format")? == value.format.number(),
        "immutable receipt conflict"
    );
    validate_facts(&saved, r, value.json.len() + value.private.len())?;
    if changed {
        crate::db::migration::capacity::adjust(
            tx,
            crate::db::migration::capacity::Delta {
                live_bytes: i64::try_from(value.json.len() + value.private.len())?
                    .checked_add(i64::try_from(super::capacity::ROW_OVERHEAD)?)
                    .context("receipt capacity overflow")?,
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(changed)
}
fn validate_facts(row: &QueryResult, facts: &format::Facts<'_>, bytes: usize) -> Result<()> {
    for (column, value) in [
        ("installation_id", facts.installation.as_str()),
        ("tenant_id", facts.tenant.as_str()),
        ("execution_id", facts.execution.as_str()),
        ("principal_id", facts.principal.as_str()),
        ("correlation_id", facts.correlation.as_str()),
        ("recorded_at", facts.recorded_at.as_str()),
    ] {
        ensure!(
            row.try_get::<String>("", column)? == value,
            "receipt projection mismatch"
        );
    }
    ensure!(
        row.try_get::<i64>("", "byte_count")? == i64::try_from(bytes)?
            && row.try_get::<bool>("", "unknown_outcome")?
                == (facts.state == ReceiptState::Unknown),
        "receipt accounting projection mismatch"
    );
    Ok(())
}
pub(super) fn validate_projection(row: &QueryResult, terminal: &format::Terminal) -> Result<()> {
    ensure!(
        row.try_get::<String>("", "content_digest")? == terminal.digest()
            && row.try_get::<i32>("", "receipt_format")? == terminal.format().number(),
        "receipt integrity mismatch"
    );
    validate_facts(row, &terminal.facts(), usize::try_from(terminal.bytes())?)
}
pub(in crate::tenancy) async fn insert_terminal(
    tx: &DatabaseTransaction,
    terminal: &format::Terminal,
) -> Result<bool> {
    match terminal {
        format::Terminal::Strict(candidate) => insert(tx, candidate).await,
        format::Terminal::Ordered(candidate) => {
            let changed = insert_bytes(
                tx,
                Row {
                    facts: terminal.facts(),
                    format: terminal.format(),
                    json: terminal.json(),
                    private: terminal.private_json(),
                    digest: terminal.digest(),
                },
            )
            .await?;
            super::usage_evidence::storage::insert_v2(tx, candidate, changed).await?;
            super::ordered::intent::retire(tx, candidate, changed).await?;
            Ok(changed)
        }
    }
}
pub(in crate::tenancy) async fn lookup_terminal(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<Option<format::Terminal>> {
    load::terminal(db, installation, tenant, id, None, false).await
}

/// Pending writes, not validated evidence or custody. Finalize after all side-writes.
#[must_use = "terminal writes require final reconciliation before commit"]
pub(in crate::tenancy) struct TerminalWrite<'a> {
    tx: &'a DatabaseTransaction,
    expected: ExpectedTerminal<'a>,
    inserted: bool,
}
enum ExpectedTerminal<'a> {
    Strict(&'a Candidate),
    Ordered(&'a super::ordered::CandidateV2),
}
impl TerminalWrite<'_> {
    pub(in crate::tenancy) fn inserted(&self) -> bool {
        self.inserted
    }
    pub(in crate::tenancy) async fn reconcile(
        self,
        charge: &archive::Charge<'_>,
    ) -> Result<obligations::Change> {
        if self.inserted
            && let ExpectedTerminal::Strict(candidate) = self.expected
        {
            return obligations::reconcile_completion(self.tx, candidate, charge).await;
        }
        let (installation, tenant, execution) = match self.expected {
            ExpectedTerminal::Strict(c) => (
                &c.receipt.installation_id,
                &c.receipt.tenant_id,
                &c.receipt.execution_id,
            ),
            ExpectedTerminal::Ordered(c) => (
                &c.receipt.installation_id,
                &c.receipt.tenant_id,
                &c.receipt.execution_id,
            ),
        };
        obligations::reconcile(
            self.tx,
            installation,
            &obligations::Key {
                tenant: tenant.clone(),
                execution: execution.clone(),
            },
            charge,
        )
        .await
    }
}

pub(in crate::tenancy) async fn stage_dispatch_terminal<'a>(
    tx: &'a DatabaseTransaction,
    terminal: &'a format::Terminal,
) -> Result<TerminalWrite<'a>> {
    match terminal {
        format::Terminal::Strict(candidate) => stage_terminal(tx, candidate).await,
        format::Terminal::Ordered(candidate) => Ok(TerminalWrite {
            tx,
            expected: ExpectedTerminal::Ordered(candidate),
            inserted: insert_terminal(tx, terminal).await?,
        }),
    }
}

/// A fresh archive-capable terminal defers readback to its final source proof.
pub(in crate::tenancy) async fn stage_terminal<'a>(
    tx: &'a DatabaseTransaction,
    candidate: &'a Candidate,
) -> Result<TerminalWrite<'a>> {
    let inserted = if tx.get_database_backend() != DatabaseBackend::MySql {
        let receipt = &candidate.receipt;
        let missing = archive::missing_receipt(
            tx,
            &receipt.installation_id,
            &receipt.tenant_id,
            &receipt.execution_id,
        )
        .await?;
        let inserted = if missing {
            insert_new_row(tx, candidate).await?
        } else {
            insert_row(tx, candidate).await?
        };
        if inserted {
            super::usage_evidence::storage::insert_fresh(tx, candidate).await?;
        }
        inserted
    } else {
        insert(tx, candidate).await?
    };
    Ok(TerminalWrite {
        tx,
        expected: ExpectedTerminal::Strict(candidate),
        inserted,
    })
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
    inventory::load(db, installation).await
}

/// Only the formal schema upgrade may derive policy quantities from all history.
pub(crate) async fn capacity_backfill(db: &impl ConnectionTrait) -> Result<Inventory> {
    let backend = db.get_database_backend();
    let query = Query::select()
        .from(Alias::new("tenancy_installation"))
        .expr_as(
            Expr::cust(format!(
                "CASE WHEN {} BETWEEN 1 AND 128 THEN installation_id END",
                octets(backend, "installation_id")
            )),
            Alias::new("installation_id"),
        )
        .limit(2)
        .to_owned();
    let rows = db.query_all(backend.build(&query)).await?;
    ensure!(rows.len() <= 1, "duplicate capacity installation");
    if let Some(row) = rows.first() {
        let installation: InstallationId =
            row.try_get::<String>("", "installation_id")?.try_into()?;
        return inventory(db, &installation).await;
    }
    // Missing identity never turns retained financial state into zero use.
    for table in [
        TABLE,
        "tenancy_usage_evidence",
        "tenancy_receipt_auxiliary",
        "tenancy_oauth_slots",
    ] {
        let query = Query::select()
            .from(Alias::new(table))
            .expr(Expr::val(1))
            .limit(1)
            .to_owned();
        ensure!(
            db.query_one(backend.build(&query)).await?.is_none(),
            "capacity source without installation"
        );
    }
    if backend != DatabaseBackend::MySql {
        for table in ["tenancy_receipt_archive", "tenancy_unknown_obligations"] {
            let query = Query::select()
                .from(Alias::new(table))
                .expr(Expr::val(1))
                .limit(1)
                .to_owned();
            ensure!(
                db.query_one(backend.build(&query)).await?.is_none(),
                "capacity source without installation"
            );
        }
        if crate::db::migration::provider_technical::available(db).await? {
            ensure!(
                super::provider_technical::storage::retained_bytes(db).await? == 0,
                "technical capacity source without installation"
            );
        }
    }
    Ok(Inventory::default())
}
