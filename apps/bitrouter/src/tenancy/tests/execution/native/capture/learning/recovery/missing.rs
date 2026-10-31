//! No deleted receipt or fabricated seed: only a real INSERT aborted by SQLite.
use super::*;
use crate::tenancy::transaction::{Disposition, receipt_observation::Key};

pub(super) async fn install(db: &sea_orm::DatabaseConnection) -> Result<()> {
    db.execute_unprepared(
        "CREATE TRIGGER paid_terminal_aborted BEFORE INSERT ON tenancy_execution_receipts \
         WHEN EXISTS (SELECT 1 FROM checkpoint_evolution \
         WHERE kind='owned_judge_active_v3' AND json_extract(body,'$.execution')=NEW.execution_id) \
         BEGIN SELECT RAISE(ABORT, 'paid terminal rollback fixture'); END",
    )
    .await?;
    Ok(())
}

pub(super) async fn absent(db: &sea_orm::DatabaseConnection, execution: &str) -> Result<()> {
    ensure!(
        db.query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM tenancy_execution_receipts WHERE execution_id=?",
            [execution.into()]
        ))
        .await?
        .is_none(),
        "producer unexpectedly committed the paid receipt"
    );
    Ok(())
}

pub(super) async fn receipt_rollback(
    db: &sea_orm::DatabaseConnection,
    service: &Service,
) -> Result<()> {
    // The parent holds the real judge response until this actual accepted seed
    // has selected the read-only receipt observer. No caller supplies a proof.
    let seed = tokio::time::timeout(TIMEOUT, async {
        loop {
            let jobs = values(db, "owned_judge_active_v3").await?;
            if let [job] = jobs.as_slice() {
                return Ok::<_, anyhow::Error>(job["recovery"]["seed"].clone());
            }
            ensure!(jobs.is_empty(), "unexpected paid owner set");
            tokio::task::yield_now().await;
        }
    })
    .await??;
    let key = Key {
        installation: seed["installation"]
            .as_str()
            .context("real seed installation")?
            .try_into()?,
        tenant: seed["tenant"]
            .as_str()
            .context("real seed tenant")?
            .try_into()?,
        execution: seed["execution"]
            .as_str()
            .context("real seed execution")?
            .try_into()?,
    };
    absent(db, key.execution.as_str()).await?;
    service.cleanup.observe_receipt_for_test(key.clone())?;
    signal("paid-recovery-accepted-before-terminal")?;
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let trace = service.cleanup.receipt_trace_for_test()?;
            ensure!(trace.key == key && !trace.overflow);
            if let Some(fact) = trace
                .facts
                .iter()
                .flatten()
                .find(|fact| fact.rollback.is_some())
            {
                ensure!(fact.begin.is_some_and(|value| value.ok));
                ensure!(
                    fact.store
                        .is_some_and(|value| !value.ok && value.sqlite_code == Some(1811)),
                    "expected actual SQLite CONSTRAINT_TRIGGER from INSERT"
                );
                ensure!(
                    fact.rollback.is_some_and(|value| value.ok)
                        && fact.commit.is_none()
                        && !fact.check_failed
                        && fact.disposition == Disposition::RolledBack,
                    "receipt absence requires confirmed rollback, not an unknown H outcome"
                );
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    ensure!(
        service.current().is_ok(),
        "known rollback must leave owner cleanup available"
    );
    service.cleanup.ensure_clean()?;
    absent(db, key.execution.as_str()).await
}

pub(super) fn repaired(before: &Value, after: &Value) -> Result<()> {
    let old = before["receipts"].as_array().context("old receipt rows")?;
    let new = after["receipts"].as_array().context("new receipt rows")?;
    ensure!(
        new.len() == old.len() + 1 && old.iter().all(|row| new.contains(row)),
        "known receipt bytes must survive alongside exactly one new terminal"
    );
    let seed = before["jobs"]
        .as_array()
        .context("actual predecessor jobs")?
        .iter()
        .find(|row| row[1] == "owned_judge_active_v3")
        .context("actual accepted job")?;
    let job: Value = serde_json::from_str(seed[3].as_str().context("accepted job bytes")?)?;
    let seed = &job["recovery"]["seed"];
    ensure!(
        job["recovery"]["stopped"].is_object()
            && job["recovery"]["observed"].is_null()
            && job["output"].is_null(),
        "actual stop set must precede crash without retained output"
    );
    ensure!(!old.iter().any(|row| row[0] == seed["execution"]));
    let row = new
        .iter()
        .find(|row| row[0] == seed["execution"])
        .context("repaired exact execution")?;
    let receipt: Value = serde_json::from_str(row[1].as_str().context("receipt JSON")?)?;
    let disposition: Value = serde_json::from_str(row[2].as_str().context("disposition JSON")?)?;
    // Paid learning seeds have no capture version. Repair cannot manufacture
    // observations or a capture contract absent from the accepted seed.
    ensure!(
        seed["usage_evidence_version"].is_null()
            && disposition["usage_evidence_digest"].is_null()
            && after["usage"] == before["usage"],
        "paid repair changed retained capture or invented missing evidence"
    );
    for (receipt_field, seed_field) in [
        ("installation_id", "installation"),
        ("tenant_id", "tenant"),
        ("principal_id", "principal"),
        ("execution_id", "execution"),
        ("correlation_id", "correlation"),
        ("execution_revision", "configuration"),
    ] {
        ensure!(
            !seed[seed_field].is_null() && receipt[receipt_field] == seed[seed_field],
            "Unknown repair changed persisted paid identity"
        );
    }
    ensure!(
        receipt["state"] == "unknown"
            && receipt["outcome"] == "unknown"
            && receipt["attempts"] == json!([])
            && disposition["dispatch"] == "unknown"
            && disposition["statuses"] == json!([])
    );
    ensure!(receipt["usage"]["provenance"] == "unknown" && receipt["usage"]["complete"] == false);
    for dimension in [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
    ] {
        ensure!(
            receipt["usage"][dimension].is_null(),
            "missing durable usage is not zero"
        );
    }
    Ok(())
}

pub(super) async fn accounting(service: &Service, db: &sea_orm::DatabaseConnection) -> Result<()> {
    ensure!(
        service
            .admission
            .receipt_capacity_for_test()
            .map_err(|code| anyhow::anyhow!("receipt capacity: {code:?}"))?
            .0
            == 0,
        "bootstrap retained a completed repair reservation"
    );
    let retained = exact(db).await?;
    let receipts = retained["receipts"]
        .as_array()
        .context("accounted receipt rows")?;
    let usage = retained["usage"]
        .as_array()
        .context("accounted usage rows")?;
    let mut captured = 0usize;
    for row in receipts {
        let receipt: Value = serde_json::from_str(row[1].as_str().context("receipt JSON")?)?;
        let disposition: Value =
            serde_json::from_str(row[2].as_str().context("disposition JSON")?)?;
        let evidence = usage.iter().find(|evidence| evidence[0] == row[0]);
        if disposition["usage_evidence_digest"].is_string() {
            let evidence = evidence.context("new receipt lost captured usage")?;
            let document: Value =
                serde_json::from_str(evidence[1].as_str().context("usage JSON")?)?;
            ensure!(
                document["capture"] == "captured",
                "live receipt capture status changed"
            );
            captured += 1;
        } else {
            ensure!(
                evidence.is_none()
                    && receipt["state"] == "unknown"
                    && receipt["outcome"] == "unknown"
                    && receipt["attempts"] == json!([]),
                "only a missing paid terminal may lack captured usage"
            );
        }
    }
    ensure!(
        captured > 0 && captured == usage.len(),
        "unbound usage evidence rows"
    );
    // Compare the production M5 observation to actual encoded storage bytes,
    // independently of Admission, Candidate and restart inventory arithmetic.
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    {
        let row = db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
            r#"WITH retained AS (
                SELECT 'receipt' AS kind, byte_count,
                    length(CAST(receipt_json AS BLOB)) + length(CAST(disposition_json AS BLOB)) AS encoded_bytes
                FROM tenancy_execution_receipts
                UNION ALL
                SELECT 'usage', byte_count, length(CAST(evidence_json AS BLOB)) FROM tenancy_usage_evidence
                UNION ALL
                SELECT 'auxiliary', length(CAST(evidence_json AS BLOB)), length(CAST(evidence_json AS BLOB)) FROM tenancy_receipt_auxiliary
            ) SELECT COUNT(*) AS rows, COALESCE(SUM(encoded_bytes),0) AS bytes,
                COALESCE(SUM(byte_count != encoded_bytes),0) AS mismatched_bytes,
                COALESCE(SUM(kind = 'receipt'),0) AS receipt_rows,
                COALESCE(SUM(kind = 'usage'),0) AS usage_rows,
                COALESCE(SUM(kind = 'auxiliary'),0) AS auxiliary_rows,
                (SELECT COUNT(*) FROM tenancy_oauth_slots) AS oauth_slots
            FROM retained"#.to_owned()))
            .await?.context("independent retained byte witness")?;
        let rows = u64::try_from(row.try_get::<i64>("", "rows")?)?;
        let bytes = u64::try_from(row.try_get::<i64>("", "bytes")?)?;
        let receipt_rows = u64::try_from(row.try_get::<i64>("", "receipt_rows")?)?;
        let usage_rows = u64::try_from(row.try_get::<i64>("", "usage_rows")?)?;
        let auxiliary_rows = u64::try_from(row.try_get::<i64>("", "auxiliary_rows")?)?;
        ensure!(
            receipt_rows == u64::try_from(receipts.len())?
                && usage_rows == u64::try_from(captured)?
        );
        ensure!(
            row.try_get::<i64>("", "mismatched_bytes")? == 0,
            "stored byte count differs from encoded bytes"
        );
        ensure!(
            row.try_get::<i64>("", "oauth_slots")? == 0,
            "unexpected OAuth accounting in static fixture"
        );
        let expected = rows
            .checked_mul(16 * 1024)
            .and_then(|overhead| bytes.checked_add(overhead))
            .context("receipt and sidecar accounting overflow")?;
        let unknown = receipts.iter().map(|row| {
            Ok(serde_json::from_str::<Value>(row[1].as_str().context("receipt JSON")?)?["state"] == "unknown")
        }).collect::<Result<Vec<_>>>()?.into_iter().filter(|unknown| *unknown).count();
        let snapshot = tokio::time::timeout(TIMEOUT, async {
            loop {
                match service.admission.observed_capacity() {
                    Ok(snapshot) => break Ok::<_, anyhow::Error>(snapshot),
                    Err(crate::telemetry::receipt_latency::capacity::ReadError::Unavailable) => {
                        tokio::task::yield_now().await;
                    }
                    Err(error) => anyhow::bail!("invalid M5 accounting observation: {error:?}"),
                }
            }
        })
        .await??;
        ensure!(
            snapshot.accounted == expected
                && snapshot.live_unknown == u64::try_from(unknown)?
                && snapshot.completion_reserved == 0
                && snapshot.potential_unknown == 0
                && snapshot.active == 0
                && snapshot.queued == 0
                && !snapshot.fenced,
            "recovery accounting mismatch: actual={snapshot:?}, expected_accounted={expected}, expected_unknown={unknown}, rows={rows}, receipt_rows={receipt_rows}, usage_rows={usage_rows}, auxiliary_rows={auxiliary_rows}, encoded_bytes={bytes}"
        );
    }
    Ok(())
}
