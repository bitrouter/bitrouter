//! Atomic immutable sidecars; bounded loads verify both directions of the binding.
use super::super::store::{self, Candidate};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    execution::{ExecutionReceipt, Outcome, ReceiptState},
    usage_evidence::{Capture, ExecutionUsageEvidenceV1, MAX_BYTES},
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend,
    sea_query::{Alias, Expr, Query, SelectStatement},
};
use sha2::{Digest, Sha256};

const TABLE: &str = "tenancy_usage_evidence";
pub(in crate::tenancy::receipts) struct Evidence {
    pub json: String,
    pub digest: String,
    pub document: ExecutionUsageEvidenceV1,
}
impl Evidence {
    pub fn new(document: ExecutionUsageEvidenceV1, receipt: &ExecutionReceipt) -> Result<Self> {
        let json = document.to_validated_json()?;
        ensure!(
            (document.capture == Capture::Captured
                || (document.capture == Capture::Unavailable
                    && receipt.state == ReceiptState::Unknown
                    && receipt.outcome == Outcome::Unknown))
                && document.installation_id == receipt.installation_id
                && document.tenant_id == receipt.tenant_id
                && document.execution_id == receipt.execution_id
                && document.attempts.len() == receipt.attempts.len(),
            "usage evidence receipt binding"
        );
        for (observed, actual) in document.attempts.iter().zip(&receipt.attempts) {
            ensure!(
                observed.attempt_id == actual.attempt_id
                    && observed.outcome == actual.outcome
                    && serde_json::to_value(&observed.target)?
                        == serde_json::to_value(&actual.target)?,
                "usage evidence attempt binding"
            );
        }
        let mut hash = Sha256::new();
        hash.update(b"bitrouter.execution.usage-evidence.v1");
        hash.update((json.len() as u64).to_be_bytes());
        hash.update(json.as_bytes());
        Ok(Self {
            json,
            digest: hex::encode(hash.finalize()),
            document,
        })
    }
}
fn scope(receipt: &ExecutionReceipt) -> SelectStatement {
    Query::select()
        .from(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("installation_id")).eq(receipt.installation_id.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(receipt.tenant_id.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(receipt.execution_id.as_str()))
        .to_owned()
}
fn octets(backend: DatabaseBackend, name: &str) -> String {
    if backend == DatabaseBackend::Sqlite {
        format!("length(CAST({name} AS BLOB))")
    } else {
        format!("octet_length({name})")
    }
}
fn integer_cell(backend: DatabaseBackend, name: &str) -> String {
    if backend == DatabaseBackend::Sqlite {
        format!("CASE WHEN typeof({name})='integer' THEN {name} ELSE NULL END")
    } else {
        name.to_owned()
    }
}

type Charge<'a> = dyn Fn(usize, usize) -> Result<()> + Sync + 'a;

/// Only scalar lengths cross this boundary. The stored byte_count is checked,
/// never used as the transfer allowance. Two rows include a duplicate sentinel.
async fn preflight(
    db: &impl ConnectionTrait,
    receipt: &ExecutionReceipt,
    expected: Option<&str>,
    charge: &Charge<'_>,
) -> Result<Option<usize>> {
    charge(2, 1024)?;
    let backend = db.get_database_backend();
    let json_bytes = octets(backend, "evidence_json");
    let cast = if backend == DatabaseBackend::MySql {
        "SIGNED"
    } else {
        "BIGINT"
    };
    let mut query = scope(receipt);
    query.expr_as(Expr::cust(format!(
        "CASE WHEN {json_bytes} BETWEEN 1 AND {MAX_BYTES} AND {}=64 AND {}=64 THEN CAST({json_bytes} AS {cast}) ELSE NULL END",
        octets(backend, "evidence_digest"), octets(backend, "receipt_digest")
    )), Alias::new("evidence_bytes"));
    for name in ["byte_count", "schema_version"] {
        query.expr_as(Expr::cust(integer_cell(backend, name)), Alias::new(name));
    }
    query.limit(2);
    let rows = db.query_all(backend.build(&query)).await?;
    charge(0, 0)?;
    ensure!(rows.len() <= 1, "usage evidence multiplicity");
    match (expected, rows.first()) {
        (None, None) => Ok(None),
        (Some(_), Some(row)) => {
            let bytes = usize::try_from(row.try_get::<i64>("", "evidence_bytes")?)?;
            ensure!(
                row.try_get::<i64>("", "byte_count")? == i64::try_from(bytes)?
                    && row.try_get::<i32>("", "schema_version")? == 1,
                "usage evidence length or version"
            );
            Ok(Some(bytes))
        }
        _ => anyhow::bail!("usage evidence presence mismatch"),
    }
}
async fn load_inline(
    db: &impl ConnectionTrait,
    receipt: &ExecutionReceipt,
    receipt_digest: &str,
    expected: Option<&str>,
    charge: Option<&Charge<'_>>,
) -> Result<Option<Evidence>> {
    let measured = if let Some(charge) = charge {
        let Some(bytes) = preflight(db, receipt, expected, charge).await? else {
            return Ok(None);
        };
        Some(bytes)
    } else {
        None
    };
    load_inline_body(db, receipt, receipt_digest, expected, charge, measured).await
}

async fn load_inline_body(
    db: &impl ConnectionTrait,
    receipt: &ExecutionReceipt,
    receipt_digest: &str,
    expected: Option<&str>,
    charge: Option<&Charge<'_>>,
    measured: Option<usize>,
) -> Result<Option<Evidence>> {
    if let (Some(charge), Some(bytes)) = (charge, measured) {
        // Three copies cover transfer, decoding and canonical encoding.
        // Both digest strings are exactly 64 bytes; fixed overhead covers
        // scalar cells and allocation metadata. A duplicate is rejected
        // before decoding, so its two transferred bodies fit this allowance.
        let allocation = bytes
            .checked_add(128)
            .and_then(|bytes| bytes.checked_mul(3))
            .and_then(|bytes| bytes.checked_add(4096))
            .context("usage evidence allocation bound")?;
        charge(2, allocation)?;
    }
    let backend = db.get_database_backend();
    let mut query = scope(receipt);
    for (name, bound) in [
        ("evidence_json", MAX_BYTES),
        ("evidence_digest", 64),
        ("receipt_digest", 64),
    ] {
        let length = octets(backend, name);
        // Keep the absolute CASE guard, and also cap transfer to the measured
        // length. Growth between queries must become NULL before allocation.
        let unchanged = measured.map_or_else(String::new, |bytes| {
            format!(
                " AND {length}={}",
                if name == "evidence_json" { bytes } else { 64 }
            )
        });
        query.expr_as(
            Expr::cust(format!(
                "CASE WHEN {length} <= {bound}{unchanged} THEN {name} ELSE NULL END"
            )),
            Alias::new(name),
        );
    }
    for name in ["byte_count", "schema_version"] {
        query.expr_as(Expr::cust(integer_cell(backend, name)), Alias::new(name));
    }
    query.limit(2);
    let rows = db.query_all(backend.build(&query)).await?;
    if let Some(charge) = charge {
        charge(0, 0)?;
    }
    ensure!(rows.len() <= 1, "usage evidence multiplicity");
    match (expected, rows.first()) {
        (None, None) => Ok(None),
        (Some(expected), Some(row)) => {
            let json: String = row.try_get("", "evidence_json")?;
            ensure!(
                measured.is_none_or(|bytes| json.len() == bytes),
                "usage evidence length changed"
            );
            let evidence = Evidence::new(serde_json::from_str(&json)?, receipt)?;
            ensure!(
                evidence.json == json
                    && evidence.digest == expected
                    && row.try_get::<String>("", "evidence_digest")? == expected
                    && row.try_get::<String>("", "receipt_digest")? == receipt_digest
                    && row.try_get::<i64>("", "byte_count")? == i64::try_from(json.len())?
                    && row.try_get::<i32>("", "schema_version")? == 1,
                "usage evidence integrity"
            );
            Ok(Some(evidence))
        }
        _ => anyhow::bail!("usage evidence presence mismatch"),
    }
}
/// The canonical reader calls only this inline path, avoiding recursion when a
/// direct sidecar reader follows an archive location back through that reader.
pub(in crate::tenancy::receipts) async fn validate_inline(
    db: &impl ConnectionTrait,
    candidate: &mut Candidate,
    expected: Option<&str>,
    charge: Option<&Charge<'_>>,
) -> Result<()> {
    let uncharged = |_, _| Ok(());
    let measured =
        store::archive::usage_metadata(db, candidate, false, charge.unwrap_or(&uncharged)).await?;
    candidate.usage_evidence = if db.get_database_backend() == DatabaseBackend::Sqlite
        && let Some(charge) = charge
    {
        // Reuse only the scalar validation just performed on this database.
        // Retain the original preflight debits/checkpoints, including absence.
        charge(2, 1024)?;
        charge(0, 0)?;
        ensure!(
            measured.is_some() == expected.is_some(),
            "usage evidence presence mismatch"
        );
        if measured.is_some() {
            load_inline_body(
                db,
                &candidate.receipt,
                &candidate.digest,
                expected,
                Some(charge),
                measured,
            )
            .await?
        } else {
            None
        }
    } else {
        load_inline(db, &candidate.receipt, &candidate.digest, expected, charge).await?
    };
    Ok(())
}

async fn load(
    db: &impl ConnectionTrait,
    receipt: &ExecutionReceipt,
    receipt_digest: &str,
    expected: Option<&str>,
    charge: Option<&Charge<'_>>,
) -> Result<Option<Evidence>> {
    if db.get_database_backend() == DatabaseBackend::Sqlite {
        if let Some(charge) = charge {
            charge(1, 256)?;
        }
        let query = Query::select()
            .from(Alias::new("tenancy_execution_receipts"))
            .expr_as(
                Expr::cust(integer_cell(DatabaseBackend::Sqlite, "storage_kind")),
                Alias::new("storage_kind"),
            )
            .and_where(
                Expr::col(Alias::new("installation_id")).eq(receipt.installation_id.as_str()),
            )
            .and_where(Expr::col(Alias::new("tenant_id")).eq(receipt.tenant_id.as_str()))
            .and_where(Expr::col(Alias::new("execution_id")).eq(receipt.execution_id.as_str()))
            .limit(1)
            .to_owned();
        if let Some(row) = db.query_one(DatabaseBackend::Sqlite.build(&query)).await? {
            match row.try_get::<i64>("", "storage_kind")? {
                0 => {}
                1 => {
                    let saved = if let Some(charge) = charge {
                        store::lookup_candidate_bounded(
                            db,
                            &receipt.installation_id,
                            &receipt.tenant_id,
                            &receipt.execution_id,
                            charge,
                        )
                        .await?
                    } else {
                        store::lookup_candidate(
                            db,
                            &receipt.installation_id,
                            &receipt.tenant_id,
                            &receipt.execution_id,
                        )
                        .await?
                    }
                    .context("archived usage receipt missing")?;
                    ensure!(
                        saved.digest == receipt_digest
                            && saved.usage_evidence.as_ref().map(|e| e.digest.as_str()) == expected,
                        "archived usage receipt binding"
                    );
                    return Ok(saved.usage_evidence);
                }
                _ => anyhow::bail!("usage receipt storage kind"),
            }
        }
    }
    load_inline(db, receipt, receipt_digest, expected, charge).await
}

#[cfg(test)]
pub(in crate::tenancy::receipts) async fn validate(
    db: &impl ConnectionTrait,
    candidate: &mut Candidate,
    expected: Option<&str>,
) -> Result<()> {
    candidate.usage_evidence =
        load(db, &candidate.receipt, &candidate.digest, expected, None).await?;
    Ok(())
}
#[cfg(all(test, feature = "core", feature = "harness"))]
pub(in crate::tenancy::receipts) async fn validate_bounded(
    db: &impl ConnectionTrait,
    candidate: &mut Candidate,
    expected: Option<&str>,
    charge: &Charge<'_>,
) -> Result<()> {
    candidate.usage_evidence = load(
        db,
        &candidate.receipt,
        &candidate.digest,
        expected,
        Some(charge),
    )
    .await?;
    Ok(())
}
/// The terminal caller must still verify the complete database candidate.
pub(in crate::tenancy::receipts) async fn insert_fresh(
    db: &impl ConnectionTrait,
    candidate: &Candidate,
) -> Result<()> {
    if let Some(evidence) = &candidate.usage_evidence {
        let receipt = &candidate.receipt;
        let fields: Vec<(&str, sea_orm::Value)> = vec![
            ("installation_id", receipt.installation_id.as_str().into()),
            ("tenant_id", receipt.tenant_id.as_str().into()),
            ("execution_id", receipt.execution_id.as_str().into()),
            ("schema_version", 1_i32.into()),
            ("receipt_digest", candidate.digest.clone().into()),
            ("evidence_digest", evidence.digest.clone().into()),
            ("evidence_json", evidence.json.clone().into()),
            ("byte_count", i64::try_from(evidence.json.len())?.into()),
        ];
        let mut query = Query::insert();
        query
            .into_table(Alias::new(TABLE))
            .columns(fields.iter().map(|(n, _)| Alias::new(*n)))
            .values(fields.into_iter().map(|(_, v)| v.into()))?;
        ensure!(
            db.execute(db.get_database_backend().build(&query))
                .await?
                .rows_affected()
                == 1,
            "usage evidence insert count"
        );
    }
    Ok(())
}
pub(in crate::tenancy::receipts) async fn insert(
    db: &impl ConnectionTrait,
    candidate: &Candidate,
    new_receipt: bool,
) -> Result<()> {
    let expected = candidate.usage_evidence.as_ref().map(|e| e.digest.as_str());
    if new_receipt {
        insert_fresh(db, candidate).await?;
    }
    let saved = load(db, &candidate.receipt, &candidate.digest, expected, None).await?;
    ensure!(
        saved.as_ref().map(|e| (&e.json, &e.digest))
            == candidate
                .usage_evidence
                .as_ref()
                .map(|e| (&e.json, &e.digest)),
        "immutable usage evidence conflict"
    );
    Ok(())
}

/// Test entrypoint for the same complete canonical scan used by startup.
#[cfg(test)]
pub(in crate::tenancy::receipts) async fn inventory(
    db: &impl ConnectionTrait,
    installation: &bitrouter_ai::tenancy::identity::InstallationId,
) -> Result<u64> {
    let mut bytes = 0_u64;
    super::super::store::inventory::scan(db, installation, |candidate| {
        if let Some(evidence) = &candidate.usage_evidence {
            bytes = bytes
                .checked_add(evidence.json.len() as u64 + 16 * 1024)
                .context("usage evidence inventory overflow")?;
        }
        Ok(())
    })
    .await?;
    Ok(bytes)
}
