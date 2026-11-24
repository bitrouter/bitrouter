//! Receipt payload relocation. The caller owns eligibility, transaction custody,
//! disk reservations and publication; relocation never releases a live owner.
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    execution::Outcome,
    identity::{ExecutionId, InstallationId, TenantId},
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseTransaction, Statement, Value,
    sea_query::{Alias, Expr, Query, SelectStatement},
};

use super::Candidate;

mod codec;
mod select;
#[cfg(test)]
mod tests;

pub(in crate::tenancy) type Charge<'a> = dyn Fn(usize, usize) -> Result<()> + Sync + 'a;
const TABLE: &str = "tenancy_receipt_archive";
#[cfg(test)]
const KEY: &str = "installation_id=? AND tenant_id=? AND execution_id=?";

/// Progress through inline history, with at most one fully decoded candidate.
/// `after` is the last inspected key; `done` only describes this transaction's
/// remaining key range. A later maintenance pass may restart from None.
pub(in crate::tenancy) struct Selection {
    pub candidate: Option<Candidate>,
    pub after: Option<super::obligations::Key>,
    pub done: bool,
}

/// Inspect at most one 32-key page using the inline-storage covering index.
/// Requires validated obligation coverage and caller-owned transaction custody.
/// The caller must check live owners again in the archive write transaction.
/// Pending paid/native tenant partitions conservatively hold receipts until
/// their own ACKs. Selection does not reserve, commit or release work.
pub(in crate::tenancy) async fn next_candidate(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    after: Option<&super::obligations::Key>,
    charge: &Charge<'_>,
) -> Result<Selection> {
    select::next(tx, installation, after, charge).await
}

#[cfg(test)]
fn values(installation: &InstallationId, tenant: &TenantId, id: &ExecutionId) -> [Value; 3] {
    [
        installation.as_str().into(),
        tenant.as_str().into(),
        id.as_str().into(),
    ]
}
fn sqlite(sql: impl Into<String>, values: impl IntoIterator<Item = Value>) -> Statement {
    Statement::from_sql_and_values(DatabaseBackend::Sqlite, sql, values)
}
fn backend(db: &impl ConnectionTrait) -> Result<DatabaseBackend> {
    let backend = db.get_database_backend();
    ensure!(
        matches!(backend, DatabaseBackend::Sqlite | DatabaseBackend::Postgres),
        "receipt archive backend unavailable"
    );
    Ok(backend)
}
fn integer(backend: DatabaseBackend, name: &str) -> String {
    if backend == DatabaseBackend::Sqlite {
        format!("CASE WHEN typeof({name})='integer' THEN {name} ELSE NULL END")
    } else {
        format!("CAST({name} AS BIGINT)")
    }
}
fn octets(backend: DatabaseBackend, name: &str) -> String {
    if backend == DatabaseBackend::Sqlite {
        format!("length(CAST({name} AS BLOB))")
    } else {
        format!("CAST(octet_length({name}) AS BIGINT)")
    }
}
fn scoped(
    table: &str,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> SelectStatement {
    Query::select()
        .from(Alias::new(table))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(tenant.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(id.as_str()))
        .to_owned()
}
async fn present(
    db: &impl ConnectionTrait,
    table: &str,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<bool> {
    let backend = backend(db)?;
    let mut query = scoped(table, installation, tenant, id);
    query.expr(Expr::val(1_i32)).limit(1);
    Ok(db.query_one(backend.build(&query)).await?.is_some())
}

/// Terminal staging only: true proves both exact-key rows are absent in
/// this transaction. False requires the full canonical replay check; an archive
/// alongside an existing receipt may be legitimate. No validity filter may
/// turn a corrupt existing row into absence, and errors never mean missing.
pub(super) async fn missing_receipt(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<bool> {
    let receipt = present(tx, super::TABLE, installation, tenant, id).await?;
    let archive = present(tx, TABLE, installation, tenant, id).await?;
    ensure!(receipt || !archive, "orphan or duplicate receipt archive");
    Ok(!receipt)
}

pub(super) async fn ensure_inline(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<()> {
    if db.get_database_backend() == DatabaseBackend::MySql {
        return Ok(());
    }
    ensure!(
        !present(db, TABLE, installation, tenant, id).await?,
        "orphan or duplicate receipt archive"
    );
    Ok(())
}

pub(super) async fn load(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
    charge: &Charge<'_>,
) -> Result<(String, String)> {
    let backend = backend(db)?;
    charge(2, 1024)?;
    let mut query = scoped(TABLE, installation, tenant, id);
    for name in ["format_version", "codec", "receipt_bytes", "usage_bytes"] {
        query.expr_as(Expr::cust(integer(backend, name)), Alias::new(name));
    }
    let kind = if backend == DatabaseBackend::Sqlite {
        "typeof(payload)='blob' AND "
    } else {
        ""
    };
    query
        .expr_as(
            Expr::cust(format!(
                "CASE WHEN {kind}{} BETWEEN 1 AND {} THEN {} ELSE NULL END",
                octets(backend, "payload"),
                codec::MAX_PAYLOAD,
                octets(backend, "payload")
            )),
            Alias::new("stored_bytes"),
        )
        .limit(2);
    let rows = db.query_all(backend.build(&query)).await?;
    ensure!(rows.len() == 1, "missing or duplicate receipt archive");
    let row = &rows[0];
    let version: i64 = row.try_get("", "format_version")?;
    let codec: i64 = row.try_get("", "codec")?;
    ensure!(
        version == 1 && (codec == 0 || codec == 1),
        "receipt archive format or codec"
    );
    let receipt_bytes = usize::try_from(row.try_get::<i64>("", "receipt_bytes")?)?;
    let usage_bytes = usize::try_from(row.try_get::<i64>("", "usage_bytes")?)?;
    let plain = codec::plaintext_len(receipt_bytes, usage_bytes)?;
    let stored = usize::try_from(row.try_get::<i64>("", "stored_bytes")?)?;
    ensure!(
        codec != 0 || stored == plain,
        "receipt archive raw metadata length"
    );
    charge(
        2,
        stored
            .checked_mul(2)
            .context("receipt archive transfer allocation")?,
    )?;
    // Guard again in the payload SELECT: growth or a type change after preflight
    // must become NULL before the database driver allocates a cell.
    let mut query = scoped(TABLE, installation, tenant, id);
    query
        .expr_as(
            Expr::cust(format!(
                "CASE WHEN {kind}{}={stored} AND format_version={version} AND codec={codec} AND receipt_bytes={receipt_bytes} AND usage_bytes={usage_bytes} THEN payload ELSE NULL END",
                octets(backend, "payload")
            )),
            Alias::new("payload"),
        )
        .limit(2);
    let rows = db.query_all(backend.build(&query)).await?;
    charge(0, 0)?;
    ensure!(rows.len() == 1, "receipt archive changed during load");
    codec::Encoded {
        version,
        codec,
        receipt_bytes,
        usage_bytes,
        payload: rows[0].try_get("", "payload")?,
    }
    .decode(charge)
}

/// Check sidecar location without loading an inline body. Archived metadata
/// remains authoritative for presence, schema, both digests and logical size.
/// Returns the required sidecar's verified length.
pub(in crate::tenancy::receipts) async fn usage_metadata(
    db: &impl ConnectionTrait,
    r: &bitrouter_ai::tenancy::execution::ExecutionReceipt,
    receipt_digest: &str,
    expected: &str,
    archived: bool,
    charge: &Charge<'_>,
) -> Result<usize> {
    let backend = backend(db)?;
    charge(2, 2048)?;
    let mut query = scoped(
        "tenancy_usage_evidence",
        &r.installation_id,
        &r.tenant_id,
        &r.execution_id,
    );
    for name in ["schema_version", "byte_count", "storage_kind"] {
        query.expr_as(Expr::cust(integer(backend, name)), Alias::new(name));
    }
    let text = if backend == DatabaseBackend::Sqlite {
        "typeof(evidence_json)='text' AND "
    } else {
        ""
    };
    query.expr_as(
        Expr::cust(format!(
            "CASE WHEN {text}{} >= 0 THEN {} ELSE NULL END",
            octets(backend, "evidence_json"),
            octets(backend, "evidence_json")
        )),
        Alias::new("inline_bytes"),
    );
    for name in ["evidence_digest", "receipt_digest"] {
        let kind = if backend == DatabaseBackend::Sqlite {
            format!("typeof({name})='text' AND ")
        } else {
            String::new()
        };
        query.expr_as(
            Expr::cust(format!(
                "CASE WHEN {kind}{}=64 THEN {name} ELSE NULL END",
                octets(backend, name)
            )),
            Alias::new(name),
        );
    }
    query.limit(2);
    let rows = db.query_all(backend.build(&query)).await?;
    charge(0, 0)?;
    ensure!(rows.len() <= 1, "usage evidence multiplicity");
    let row = rows.first().context("missing usage evidence")?;
    ensure!(
        row.try_get::<i64>("", "schema_version")? == 1
            && row.try_get::<i64>("", "storage_kind")? == i64::from(archived)
            && row.try_get::<String>("", "evidence_digest")? == expected
            && row.try_get::<String>("", "receipt_digest")? == receipt_digest,
        "usage evidence archive metadata"
    );
    let bytes = usize::try_from(row.try_get::<i64>("", "byte_count")?)?;
    ensure!(
        (1..=bitrouter_ai::tenancy::usage_evidence::EvidenceBudget::STRICT.max_bytes)
            .contains(&bytes),
        "usage evidence archive length"
    );
    let inline = usize::try_from(row.try_get::<i64>("", "inline_bytes")?)?;
    ensure!(
        inline == if archived { 0 } else { bytes },
        "inline usage evidence length"
    );
    Ok(bytes)
}

pub(super) fn known(candidate: &Candidate) -> Result<()> {
    // Unclean also includes known NotSent/Rejected effects. Preserve those
    // unchanged; only unknown effects fail this receipt-local predicate.
    ensure!(
        !candidate.unknown()
            && candidate.receipt.outcome != Outcome::Unknown
            && candidate
                .receipt
                .attempts
                .iter()
                .all(|attempt| attempt.outcome != Outcome::Unknown)
            && !candidate.auxiliary_unknown(),
        "unresolved receipt cannot be archived"
    );
    Ok(())
}

/// Bounded compression prepared outside the publication gate. Retain the
/// caller's maintenance reservation until commit/rollback is confirmed.
pub(in crate::tenancy) struct Prepared {
    installation: InstallationId,
    tenant: TenantId,
    execution: ExecutionId,
    digest: String,
    source_bytes: u64,
    #[cfg(test)]
    index_bytes: u64,
    history_bytes: u64,
    encoded: codec::Encoded,
}
impl Prepared {
    /// The candidate must come from the canonical store reader. This only checks
    /// receipt-local eligibility; the writer must also check live owners and
    /// unresolved obligations in the transaction used by `archive_one`.
    pub(in crate::tenancy) fn new(candidate: &Candidate, charge: &Charge<'_>) -> Result<Self> {
        known(candidate)?;
        let source_bytes = candidate
            .bytes()
            .checked_add(candidate.auxiliary_bytes())
            .and_then(|n| n.checked_add(super::super::capacity::ROW_OVERHEAD))
            .context("archive source estimate overflow")?;
        let index_bytes = u64::try_from(candidate.private_json.len())?
            .checked_add(
                candidate
                    .auxiliary
                    .as_ref()
                    .map_or(0, |e| e.json.len() as u64),
            )
            .and_then(|n| n.checked_add(super::super::capacity::ROW_OVERHEAD))
            .context("archive retained index estimate overflow")?;
        let encoded =
            codec::Encoded::encode(&candidate.json, &candidate.usage_evidence.json, charge)?;
        let history_bytes = index_bytes
            .checked_add(u64::try_from(encoded.payload.len())?)
            .context("archive history estimate overflow")?;
        Ok(Self {
            installation: candidate.receipt.installation_id.clone(),
            tenant: candidate.receipt.tenant_id.clone(),
            execution: candidate.receipt.execution_id.clone(),
            digest: candidate.digest.clone(),
            source_bytes,
            #[cfg(test)]
            index_bytes,
            history_bytes,
            encoded,
        })
    }
    /// Original inline inventory charge, including sidecars and row overhead.
    /// Debit only once after confirmed relocation; this is not disk space freed.
    pub(in crate::tenancy) fn source_bytes(&self) -> u64 {
        self.source_bytes
    }
    /// Retained disposition, inline auxiliary bytes and index/row allowance.
    /// Add `stored_bytes()` for the archive's complete history estimate.
    #[cfg(test)]
    pub(in crate::tenancy) fn index_bytes(&self) -> u64 {
        self.index_bytes
    }
    /// Complete history charge retained after dropping the canonical candidate:
    /// encoded payload, disposition, inline auxiliary JSON and row overhead.
    pub(in crate::tenancy) fn history_bytes(&self) -> u64 {
        self.history_bytes
    }
    pub(in crate::tenancy) fn stored_bytes(&self) -> usize {
        self.encoded.payload.len()
    }
    pub(in crate::tenancy) fn relocated_bytes(&self) -> usize {
        self.encoded.receipt_bytes + self.encoded.usage_bytes
    }
}

/// Relocate one immutable payload within the caller's transaction. Returns false
/// for an already archived exact receipt or a durable owner holding it. The
/// caller must serialize/recheck live owner eligibility and roll back on any
/// error. This function neither commits nor publishes accounting or authority.
pub(in crate::tenancy) async fn archive_one(
    tx: &DatabaseTransaction,
    prepared: &Prepared,
    charge: &Charge<'_>,
) -> Result<bool> {
    let backend = backend(tx)?;
    let saved = super::lookup_candidate_bounded(
        tx,
        &prepared.installation,
        &prepared.tenant,
        &prepared.execution,
        charge,
    )
    .await?
    .context("receipt to archive missing")?;
    ensure!(
        saved.digest == prepared.digest,
        "receipt to archive changed"
    );
    known(&saved)?;
    let r = &saved.receipt;
    let mut marker = scoped(
        super::TABLE,
        &r.installation_id,
        &r.tenant_id,
        &r.execution_id,
    );
    marker.expr_as(
        Expr::cust(integer(backend, "storage_kind")),
        Alias::new("storage_kind"),
    );
    let row = tx
        .query_one(backend.build(&marker))
        .await?
        .context("receipt archive marker missing")?;
    match row.try_get::<i64>("", "storage_kind")? {
        1 => return Ok(false),
        0 => {}
        _ => anyhow::bail!("receipt storage kind"),
    }
    let key = super::obligations::Key {
        tenant: r.tenant_id.clone(),
        execution: r.execution_id.clone(),
    };
    if super::obligations::lookup(tx, &r.installation_id, &key, charge)
        .await?
        .is_some()
    {
        return Ok(false);
    }
    let encoded = &prepared.encoded;
    ensure!(
        encoded.receipt_bytes == saved.json.len()
            && encoded.usage_bytes == saved.usage_evidence.json.len(),
        "receipt archive preparation mismatch"
    );
    charge(
        3,
        prepared
            .stored_bytes()
            .checked_add(4096)
            .context("receipt archive write allocation")?,
    )?;
    let insert = Query::insert()
        .into_table(Alias::new(TABLE))
        .columns(
            [
                "installation_id",
                "tenant_id",
                "execution_id",
                "format_version",
                "codec",
                "receipt_bytes",
                "usage_bytes",
                "payload",
            ]
            .map(Alias::new),
        )
        .values([
            r.installation_id.as_str().into(),
            r.tenant_id.as_str().into(),
            r.execution_id.as_str().into(),
            encoded.version.into(),
            encoded.codec.into(),
            i64::try_from(encoded.receipt_bytes)?.into(),
            i64::try_from(encoded.usage_bytes)?.into(),
            encoded.payload.clone().into(),
        ])?
        .to_owned();
    ensure!(
        tx.execute(backend.build(&insert)).await?.rows_affected() == 1,
        "receipt archive insert count"
    );
    let mut usage = Query::update();
    usage
        .table(Alias::new("tenancy_usage_evidence"))
        .values([
            (Alias::new("evidence_json"), "".into()),
            (Alias::new("storage_kind"), 1_i32.into()),
        ])
        .and_where(Expr::col(Alias::new("installation_id")).eq(r.installation_id.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(r.tenant_id.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(r.execution_id.as_str()))
        .and_where(Expr::col(Alias::new("storage_kind")).eq(0_i32));
    ensure!(
        tx.execute(backend.build(&usage)).await?.rows_affected() == 1,
        "receipt archive usage update count"
    );
    let mut receipt = Query::update();
    receipt
        .table(Alias::new(super::TABLE))
        .values([
            (Alias::new("receipt_json"), "".into()),
            (Alias::new("storage_kind"), 1_i32.into()),
        ])
        .and_where(Expr::col(Alias::new("installation_id")).eq(r.installation_id.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(r.tenant_id.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(r.execution_id.as_str()))
        .and_where(Expr::col(Alias::new("storage_kind")).eq(0_i32));
    ensure!(
        tx.execute(backend.build(&receipt)).await?.rows_affected() == 1,
        "receipt archive marker update count"
    );
    crate::db::migration::capacity::adjust(
        tx,
        crate::db::migration::capacity::Delta {
            live_bytes: -i64::try_from(prepared.source_bytes())?,
            history_bytes: i64::try_from(prepared.history_bytes())?,
            history_rows: 1,
            ..Default::default()
        },
    )
    .await?;
    charge(0, 0)?;
    Ok(true)
}
