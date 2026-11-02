//! SQLite payload relocation. The caller owns eligibility, transaction custody,
//! disk reservations and publication; relocation never releases a live owner.
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    execution::Outcome,
    identity::{ExecutionId, InstallationId, TenantId},
};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseTransaction, Statement, Value};

use super::Candidate;

mod codec;
mod select;
#[cfg(test)]
mod tests;

pub(in crate::tenancy) type Charge<'a> = dyn Fn(usize, usize) -> Result<()> + Sync + 'a;
const TABLE: &str = "tenancy_receipt_archive";
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
fn integer(name: &str) -> String {
    format!("CASE WHEN typeof({name})='integer' THEN {name} ELSE NULL END AS {name}")
}

/// SQLite terminal staging only: true proves both exact-key rows are absent in
/// this transaction. False requires the full canonical replay check; an archive
/// alongside an existing receipt may be legitimate. No validity filter may
/// turn a corrupt existing row into absence, and errors never mean missing.
pub(super) async fn missing_receipt(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<bool> {
    ensure!(
        tx.get_database_backend() == DatabaseBackend::Sqlite,
        "receipt presence probe requires SQLite"
    );
    let row = tx
        .query_one(sqlite(
            format!(
                "SELECT EXISTS(SELECT 1 FROM {} WHERE {KEY}) AS receipt_present, EXISTS(SELECT 1 FROM {TABLE} WHERE {KEY}) AS archive_present",
                super::TABLE
            ),
            values(installation, tenant, id)
                .into_iter()
                .chain(values(installation, tenant, id)),
        ))
        .await?
        .context("receipt presence result missing")?;
    let receipt: bool = row.try_get("", "receipt_present")?;
    let archive: bool = row.try_get("", "archive_present")?;
    ensure!(receipt || !archive, "orphan or duplicate receipt archive");
    Ok(!receipt)
}

pub(super) async fn ensure_inline(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<()> {
    if db.get_database_backend() != DatabaseBackend::Sqlite {
        return Ok(());
    }
    ensure!(
        db.query_one(sqlite(
            format!("SELECT 1 FROM {TABLE} WHERE {KEY} LIMIT 1"),
            values(installation, tenant, id)
        ))
        .await?
        .is_none(),
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
) -> Result<(String, Option<String>)> {
    ensure!(
        db.get_database_backend() == DatabaseBackend::Sqlite,
        "receipt archive requires SQLite"
    );
    charge(2, 1024)?;
    let columns = ["format_version", "codec", "receipt_bytes", "usage_bytes"]
        .map(integer)
        .join(",");
    let rows = db.query_all(sqlite(format!(
        "SELECT {columns}, CASE WHEN typeof(payload)='blob' AND length(payload) BETWEEN 1 AND {} THEN length(payload) ELSE NULL END AS stored_bytes FROM {TABLE} WHERE {KEY} LIMIT 2",
        codec::MAX_PAYLOAD), values(installation, tenant, id))).await?;
    ensure!(rows.len() == 1, "missing or duplicate receipt archive");
    let row = &rows[0];
    let version: i64 = row.try_get("", "format_version")?;
    let codec: i64 = row.try_get("", "codec")?;
    ensure!(
        version == 1 && (codec == 0 || codec == 1),
        "receipt archive format or codec"
    );
    let receipt_bytes = usize::try_from(row.try_get::<i64>("", "receipt_bytes")?)?;
    let usage = usize::try_from(row.try_get::<i64>("", "usage_bytes")?)?;
    let usage_bytes = (usage != 0).then_some(usage);
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
    let rows = db.query_all(sqlite(format!(
        "SELECT CASE WHEN typeof(payload)='blob' AND length(payload)={stored} AND format_version={version} AND codec={codec} AND receipt_bytes={receipt_bytes} AND usage_bytes={usage} THEN payload ELSE NULL END AS payload FROM {TABLE} WHERE {KEY} LIMIT 2"
    ), values(installation, tenant, id))).await?;
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
/// SQLite returns validated body length or absence. Other backends retain the
/// no-op path; callers must not interpret its None as verified absence.
pub(in crate::tenancy::receipts) async fn usage_metadata(
    db: &impl ConnectionTrait,
    candidate: &Candidate,
    archived: bool,
    charge: &Charge<'_>,
) -> Result<Option<usize>> {
    if db.get_database_backend() != DatabaseBackend::Sqlite {
        ensure!(!archived, "receipt archive requires SQLite");
        return Ok(None);
    }
    charge(2, 2048)?;
    let r = &candidate.receipt;
    let columns = ["schema_version", "byte_count", "storage_kind"]
        .map(integer)
        .join(",");
    let rows = db.query_all(sqlite(format!(
        "SELECT {columns}, CASE WHEN typeof(evidence_json)='text' THEN length(CAST(evidence_json AS BLOB)) ELSE NULL END AS inline_bytes, CASE WHEN typeof(evidence_digest)='text' AND length(CAST(evidence_digest AS BLOB))=64 THEN evidence_digest ELSE NULL END AS evidence_digest, CASE WHEN typeof(receipt_digest)='text' AND length(CAST(receipt_digest AS BLOB))=64 THEN receipt_digest ELSE NULL END AS receipt_digest FROM tenancy_usage_evidence WHERE {KEY} LIMIT 2"
    ), values(&r.installation_id, &r.tenant_id, &r.execution_id))).await?;
    charge(0, 0)?;
    ensure!(rows.len() <= 1, "usage evidence multiplicity");
    let disposition: super::StoredDisposition = serde_json::from_str(&candidate.private_json)?;
    match (disposition.usage_evidence_digest.as_deref(), rows.first()) {
        (None, None) => {
            ensure!(
                candidate.usage_evidence.is_none(),
                "unexpected archived usage"
            );
            Ok(None)
        }
        (Some(expected), Some(row)) => {
            ensure!(
                row.try_get::<i64>("", "schema_version")? == 1
                    && row.try_get::<i64>("", "storage_kind")? == i64::from(archived)
                    && row.try_get::<String>("", "evidence_digest")? == expected
                    && row.try_get::<String>("", "receipt_digest")? == candidate.digest,
                "usage evidence archive metadata"
            );
            let bytes = usize::try_from(row.try_get::<i64>("", "byte_count")?)?;
            ensure!(
                (1..=bitrouter_ai::tenancy::usage_evidence::MAX_BYTES).contains(&bytes),
                "usage evidence archive length"
            );
            let inline = usize::try_from(row.try_get::<i64>("", "inline_bytes")?)?;
            if archived {
                let evidence = candidate
                    .usage_evidence
                    .as_ref()
                    .context("missing archived usage")?;
                ensure!(
                    inline == 0 && evidence.json.len() == bytes && evidence.digest == expected,
                    "archived usage evidence integrity"
                );
            } else {
                ensure!(inline == bytes, "inline usage evidence length");
            }
            Ok(Some(bytes))
        }
        _ => anyhow::bail!("usage evidence presence mismatch"),
    }
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
        let encoded = codec::Encoded::encode(
            &candidate.json,
            candidate.usage_evidence.as_ref().map(|e| e.json.as_str()),
            charge,
        )?;
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
        self.encoded.receipt_bytes + self.encoded.usage_bytes.unwrap_or(0)
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
    ensure!(
        tx.get_database_backend() == DatabaseBackend::Sqlite,
        "receipt archive requires SQLite"
    );
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
    let row = tx
        .query_one(sqlite(
            format!(
                "SELECT {} FROM {} WHERE {KEY}",
                integer("storage_kind"),
                super::TABLE
            ),
            values(&r.installation_id, &r.tenant_id, &r.execution_id),
        ))
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
        || select::held(tx, &saved, charge).await?
    {
        return Ok(false);
    }
    let encoded = &prepared.encoded;
    ensure!(
        encoded.receipt_bytes == saved.json.len()
            && encoded.usage_bytes == saved.usage_evidence.as_ref().map(|e| e.json.len()),
        "receipt archive preparation mismatch"
    );
    charge(
        3,
        prepared
            .stored_bytes()
            .checked_add(4096)
            .context("receipt archive write allocation")?,
    )?;
    let mut parameters = values(&r.installation_id, &r.tenant_id, &r.execution_id).to_vec();
    parameters.extend([
        encoded.version.into(),
        encoded.codec.into(),
        i64::try_from(encoded.receipt_bytes)?.into(),
        i64::try_from(encoded.usage_bytes.unwrap_or(0))?.into(),
        encoded.payload.clone().into(),
    ]);
    ensure!(tx.execute(sqlite(format!("INSERT INTO {TABLE} (installation_id,tenant_id,execution_id,format_version,codec,receipt_bytes,usage_bytes,payload) VALUES (?,?,?,?,?,?,?,?)"), parameters)).await?.rows_affected() == 1,
        "receipt archive insert count");
    if saved.usage_evidence.is_some() {
        ensure!(tx.execute(sqlite(format!("UPDATE tenancy_usage_evidence SET evidence_json='',storage_kind=1 WHERE {KEY} AND storage_kind=0"),
            values(&r.installation_id, &r.tenant_id, &r.execution_id))).await?.rows_affected() == 1,
            "receipt archive usage update count");
    }
    ensure!(
        tx.execute(sqlite(
            format!(
                "UPDATE {} SET receipt_json='',storage_kind=1 WHERE {KEY} AND storage_kind=0",
                super::TABLE
            ),
            values(&r.installation_id, &r.tenant_id, &r.execution_id)
        ))
        .await?
        .rows_affected()
            == 1,
        "receipt archive marker update count"
    );
    charge(0, 0)?;
    Ok(true)
}
