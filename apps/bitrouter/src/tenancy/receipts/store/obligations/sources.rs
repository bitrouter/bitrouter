//! Canonical classification and indexed source keys. No stored summary flag is
//! trusted instead of the receipt/auxiliary/OAuth metadata it describes.
use super::*;
use crate::tenancy::{
    oauth::storage as oauth,
    receipts::auxiliary::{Outcome, Snapshot},
};
use bitrouter_ai::tenancy::execution::Outcome as ExecutionOutcome;

pub(super) struct Sources {
    pub mask: u8,
    pub first: Option<DateTime<Utc>>,
}
impl Sources {
    fn add(&mut self, flag: u8, at: DateTime<Utc>) {
        self.mask |= flag;
        self.first = Some(self.first.map_or(at, |first| first.min(at)));
    }
}

// Same exact immutable binding as auxiliary_store::pending_receipted, evaluated
// against the already charged canonical candidate rather than reloading it.
fn witnessed(
    candidate: Option<&Candidate>,
    metadata: &oauth::Metadata,
    work: &oauth::Work,
) -> Result<bool> {
    let Some(candidate) = candidate else {
        return Ok(false);
    };
    let Some(evidence) = &candidate.auxiliary else {
        return Ok(false);
    };
    let snapshot: Snapshot = serde_json::from_str(&evidence.json)?;
    let Snapshot::Refresh(facts) = &snapshot else {
        anyhow::bail!("pending OAuth matched non-refresh evidence");
    };
    let fact = facts.first().context("pending OAuth evidence missing")?;
    ensure!(
        candidate.receipt.principal_id == work.principal
            && fact.credential == metadata.reference
            && fact.slot == metadata.slot
            && fact.work == work.id
            && fact.generation == metadata.generation
            && fact.generation == work.expected_generation
            && matches!(fact.outcome, Outcome::Unknown | Outcome::Rejected),
        "pending OAuth receipt binding mismatch"
    );
    Ok(true)
}
pub(super) async fn read(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    key: &Key,
    charge: &Charge<'_>,
) -> Result<Sources> {
    let candidate = super::super::lookup_candidate_bounded(
        db,
        installation,
        &key.tenant,
        &key.execution,
        charge,
    )
    .await?;
    let mut sources = Sources {
        mask: 0,
        first: None,
    };
    if let Some(candidate) = &candidate {
        let at = DateTime::parse_from_rfc3339(candidate.receipt.recorded_at.as_str())?
            .with_timezone(&Utc);
        if candidate.unknown()
            || candidate.receipt.outcome == ExecutionOutcome::Unknown
            || candidate
                .receipt
                .attempts
                .iter()
                .any(|attempt| attempt.outcome == ExecutionOutcome::Unknown)
        {
            sources.add(RECEIPT, at);
        }
        if candidate.auxiliary_unknown() {
            sources.add(AUXILIARY, at);
        }
    } else {
        charge(2, 1024)?;
        for table in ["tenancy_usage_evidence", "tenancy_receipt_auxiliary"] {
            ensure!(db.query_one(sql(format!("SELECT 1 FROM {table} WHERE installation_id=? AND tenant_id=? AND execution_id=? LIMIT 1"), [installation.as_str().into(), key.tenant.as_str().into(), key.execution.as_str().into()])).await?.is_none(), "orphan unknown source evidence");
        }
    }
    let mut after: Option<String> = None;
    loop {
        let page = oauth::pending::for_execution(
            db,
            installation,
            &key.tenant,
            &key.execution,
            after.as_deref(),
            charge,
        )
        .await?;
        if page.is_empty() {
            break;
        }
        for metadata in page {
            let work = match &metadata.state {
                oauth::State::RefreshStarted(work) | oauth::State::Quarantined(work) => work,
                oauth::State::Ready => anyhow::bail!("Ready OAuth pending source"),
            };
            charge(0, crate::tenancy::receipts::auxiliary::MAX_BYTES * 2)?;
            if !witnessed(candidate.as_ref(), &metadata, work)? {
                sources.add(
                    PENDING,
                    DateTime::parse_from_rfc3339(work.started_at.as_str())?.with_timezone(&Utc),
                );
            }
            // Do not stop at the first source: later slots may be corrupt.
            after = Some(metadata.slot);
        }
    }
    charge(0, 0)?;
    Ok(sources)
}

/// Canonical receipt keys for complete validation/backfill, including known
/// receipts. Caller retains the final key and a stable read/exclusive writer.
pub(in crate::tenancy) async fn receipt_keys(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    after: Option<&Key>,
    charge: &Charge<'_>,
) -> Result<Vec<Key>> {
    keys(db, installation, after, false, charge).await
}
/// Distinct pending execution keys through the execution index. This does not
/// replace full OAuth slot validation, which also inspects falsely null rows.
pub(in crate::tenancy) async fn pending_keys(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    after: Option<&Key>,
    charge: &Charge<'_>,
) -> Result<Vec<Key>> {
    keys(db, installation, after, true, charge).await
}
async fn keys(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    after: Option<&Key>,
    pending: bool,
    charge: &Charge<'_>,
) -> Result<Vec<Key>> {
    sqlite(db)?;
    charge(PAGE, PAGE * 512)?;
    let (table, tenant, execution) = if pending {
        (
            "tenancy_oauth_slots",
            "pending_tenant_id",
            "pending_execution_id",
        )
    } else {
        ("tenancy_execution_receipts", "tenant_id", "execution_id")
    };
    // DISTINCT and ordering operate on the raw covering-index columns; guarded
    // projections are decoded below and never silently omitted as invalid keys.
    let mut query = format!(
        "SELECT DISTINCT {tenant} AS tenant_id,{execution} AS execution_id FROM {table} WHERE installation_id=?"
    );
    let mut values: Vec<Value> = vec![installation.as_str().into()];
    if pending {
        query.push_str(" AND pending_tenant_id IS NOT NULL AND pending_execution_id IS NOT NULL");
    }
    if let Some(after) = after {
        query.push_str(&format!(" AND ({tenant},{execution})>(?,?)"));
        values.extend([
            after.tenant.as_str().into(),
            after.execution.as_str().into(),
        ]);
    }
    query.push_str(&format!(" ORDER BY {tenant},{execution} LIMIT ?"));
    values.push(i64::try_from(PAGE)?.into());
    // Guard before transfer; only a bounded inner key page reaches these CASEs.
    let query = format!(
        "SELECT CASE WHEN typeof(tenant_id)='text' AND length(CAST(tenant_id AS BLOB)) BETWEEN 1 AND 128 THEN tenant_id ELSE NULL END AS tenant_id,CASE WHEN typeof(execution_id)='text' AND length(CAST(execution_id AS BLOB)) BETWEEN 1 AND 128 THEN execution_id ELSE NULL END AS execution_id FROM ({query})"
    );
    let rows = db.query_all(sql(query, values)).await?;
    let mut result = Vec::new();
    result.try_reserve_exact(rows.len())?;
    let mut previous = after.cloned();
    for row in rows {
        let key = Key {
            tenant: row.try_get::<String>("", "tenant_id")?.try_into()?,
            execution: row.try_get::<String>("", "execution_id")?.try_into()?,
        };
        ensure!(
            previous.as_ref().is_none_or(|old| old < &key),
            "unknown source key order"
        );
        previous = Some(key.clone());
        result.push(key);
    }
    charge(0, 0)?;
    Ok(result)
}
