use super::*;
use crate::tenancy::receipts::{
    auxiliary, auxiliary_store,
    store::{self as receipts, Candidate, Disposition, obligations},
    usage_evidence::Snapshot as Evidence,
};
use bitrouter_ai::tenancy::{
    execution::{Attempt, ExecutionReceipt, Outcome, Provenance, ReceiptState, Target, Usage},
    identity::ExecutionId,
    operation::DispatchState,
    usage_evidence::{Protocol, Source},
};
use sea_orm::TransactionTrait;
use sha2::{Digest, Sha256};

mod unknown;

fn usage(n: u64) -> Usage {
    Usage {
        input_tokens: Some(n),
        output_tokens: Some(n),
        cache_read_tokens: Some(n),
        cache_write_tokens: Some(n),
        reasoning_tokens: Some(n),
        provenance: Provenance::Provider,
        complete: true,
    }
}

fn candidate(h: &Harness, reference: &ResourceRef, id: &str) -> Result<Candidate> {
    let mut receipt = ExecutionReceipt {
        installation_id: h.fixture.installation.clone(),
        tenant_id: h.fixture.tenant.clone(),
        principal_id: h.fixture.principal.clone(),
        execution_id: id.try_into()?,
        correlation_id: "history-correlation".try_into()?,
        execution_revision: "history-revision".try_into()?,
        state: ReceiptState::Committed,
        outcome: Outcome::Completed,
        attempts: Vec::new(),
        usage: usage(8),
        recorded_at: "2026-10-25T00:00:00Z".try_into()?,
    };
    let mut evidence = Evidence::default();
    for index in 0..8 {
        let attempt = Attempt {
            attempt_id: format!("attempt-{index}-{}", "a".repeat(112)).try_into()?,
            target: Some(Target {
                provider_id: "synthetic-provider".try_into()?,
                model_id: "synthetic-model".into(),
                account_id: "synthetic-account".try_into()?,
                credential_id: reference.resource_id.clone().as_str().try_into()?,
                credential_version: reference.revision_id.clone(),
            }),
            outcome: Outcome::Completed,
            usage: usage(1),
            native_usage: Vec::new(),
        };
        for frame in 0..32 {
            evidence.observe(&attempt.attempt_id, Protocol::ChatCompletions, Source::ChatChunk, &serde_json::json!({
                "prompt_tokens": frame, "completion_tokens": frame, "total_tokens": 2 * frame,
                "prompt_tokens_details": {"cached_tokens": 1, "audio_tokens": 1, "image_tokens": 1},
                "completion_tokens_details": {"reasoning_tokens": 1, "audio_tokens": 1, "image_tokens": 1}
            }));
        }
        evidence.terminals.push(attempt.attempt_id.clone());
        receipt.attempts.push(attempt);
    }
    let candidate = Candidate::new(
        receipt,
        Disposition {
            dispatch: DispatchState::Started,
            statuses: vec![Some(200); 8],
            auxiliary_digest: None,
        },
    )?
    .with_usage_evidence(evidence)?;
    ensure!(usage_bytes(&candidate)? >= 128 * 1024);
    Ok(candidate)
}

fn id(index: u64) -> String {
    format!("history-{index:010}")
}

#[derive(Default)]
struct Ledger {
    rows: u64,
    payload: u64,
    canonical: u64,
    hash: Sha256,
}

fn usage_bytes(candidate: &Candidate) -> Result<u64> {
    // Known fixture rows have one usage sidecar and no refresh/count sidecar.
    // auxiliary_bytes includes this fixed per-sidecar accounting allowance.
    // SQL length checks below independently witness the actual inline payload.
    candidate
        .auxiliary_bytes()
        .checked_sub(16 * 1024)
        .context("usage sidecar absent")
}

impl Ledger {
    fn add(&mut self, candidate: &Candidate) -> Result<()> {
        // The production reader recomputes this immutable commitment from exact
        // receipt/disposition bytes and verifies the bound sidecar digests.
        for value in [
            candidate.receipt.installation_id.as_str(),
            candidate.receipt.tenant_id.as_str(),
            candidate.receipt.execution_id.as_str(),
            &candidate.digest,
        ] {
            self.hash.update(u64::try_from(value.len())?.to_be_bytes());
            self.hash.update(value.as_bytes());
        }
        self.rows = self.rows.checked_add(1).context("receipt rows")?;
        self.payload = self
            .payload
            .checked_add(
                u64::try_from(serde_json::to_vec(&candidate.receipt)?.len())?
                    + usage_bytes(candidate)?,
            )
            .context("payload bytes")?;
        self.canonical = self
            .canonical
            .checked_add(candidate.bytes() + candidate.auxiliary_bytes())
            .context("canonical bytes")?;
        Ok(())
    }
    fn digest(&self) -> String {
        hex::encode(self.hash.clone().finalize())
    }
}

async fn insert(tx: &sea_orm::DatabaseTransaction, value: &Candidate) -> Result<()> {
    ensure!(receipts::insert(tx, value).await?);
    auxiliary_store::insert(tx, value).await?;
    let change = obligations::reconcile(
        tx,
        &value.receipt.installation_id,
        &obligations::Key {
            tenant: value.receipt.tenant_id.clone(),
            execution: value.receipt.execution_id.clone(),
        },
        &|_, _| Ok(()),
    )
    .await?;
    drop(change);
    Ok(())
}

async fn append(
    h: &Harness,
    reference: &ResourceRef,
    ledger: &mut Ledger,
    bytes: u64,
) -> Result<()> {
    let before = ledger.payload;
    let start = std::time::Instant::now();
    let first_row = ledger.rows;
    let mut reported = 0;
    while if bytes == 0 {
        ledger.rows - first_row < 65
    } else {
        ledger.payload - before < bytes
    } {
        disk::require_free(h.path.parent().context("database directory")?, 2 * GIB)?;
        let tx = h.fixture.db.begin().await?;
        let mut batch_bytes = 0;
        while batch_bytes < 8 * 1024 * 1024 {
            if (bytes == 0 && ledger.rows - first_row >= 65)
                || (bytes > 0 && ledger.payload - before >= bytes)
            {
                break;
            }
            let value = candidate(h, reference, &id(ledger.rows))?;
            insert(&tx, &value).await?;
            batch_bytes += value.bytes() + value.auxiliary_bytes();
            ledger.add(&value)?;
        }
        tx.commit().await?;
        // Bound WAL growth during accelerated setup; no history-wide transaction.
        disk::checkpoint(h).await?;
        if (ledger.rows - first_row) / 512 > reported {
            reported = (ledger.rows - first_row) / 512;
            eprintln!(
                "history seed rows={} actual_payload_bytes={} elapsed={:?}",
                ledger.rows,
                ledger.payload,
                start.elapsed()
            );
        }
    }
    Ok(())
}

async fn verify(
    h: &Harness,
    expected: &Ledger,
    unknown: &unknown::Retained,
    served: &[Candidate],
    archived: bool,
) -> Result<()> {
    let tx = h.fixture.db.begin().await?;
    let mut actual = Ledger::default();
    let mut after = String::new();
    loop {
        let page = tx.query_all(Statement::from_sql_and_values(sea_orm::DatabaseBackend::Sqlite,
            "SELECT execution_id FROM tenancy_execution_receipts WHERE installation_id=? AND tenant_id=? AND execution_id>? AND execution_id LIKE 'history-%' ORDER BY execution_id LIMIT 32",
            [h.fixture.installation.as_str().into(), h.fixture.tenant.as_str().into(), after.clone().into()])).await?;
        if page.is_empty() {
            break;
        }
        for row in page {
            let key: String = row.try_get("", "execution_id")?;
            ensure!(key > after);
            let value = receipts::lookup_candidate(
                &tx,
                &h.fixture.installation,
                &h.fixture.tenant,
                &key.as_str().try_into()?,
            )
            .await?
            .context("canonical history row")?;
            actual.add(&value)?;
            after = key;
        }
    }
    ensure!(
        actual.rows == expected.rows
            && actual.payload == expected.payload
            && actual.canonical == expected.canonical
            && actual.digest() == expected.digest()
    );
    let inventory = receipts::inventory(&tx, &h.fixture.installation).await?;
    ensure!(
        inventory.rows == expected.rows + u64::try_from(unknown.receipts.len() + served.len())?
    );
    ensure!(
        inventory.unknown_count == unknown.inventory.unknown_count
            && inventory.unknown_digest == unknown.inventory.digest
    );
    let unknown_bytes = unknown.receipts.iter().try_fold(0_u64, |sum, value| {
        sum.checked_add(value.bytes() + value.auxiliary_bytes())
            .context("unknown bytes")
    })?;
    let served_bytes = served.iter().try_fold(0_u64, |sum, value| {
        sum.checked_add(value.bytes() + value.auxiliary_bytes())
            .context("served bytes")
    })?;
    ensure!(inventory.bytes == expected.canonical + unknown_bytes + served_bytes);
    let overhead = crate::tenancy::receipts::capacity::ROW_OVERHEAD;
    ensure!(
        inventory.live_bytes
            == if archived {
                unknown_bytes + u64::try_from(unknown.receipts.len())? * overhead
            } else {
                inventory.bytes + inventory.rows * overhead
            }
    );
    ensure!(inventory.oauth_bytes > 0);
    ensure!(if archived {
        inventory.history_bytes > 0
    } else {
        inventory.history_bytes == 0
    });
    ensure!(
        inventory.history_rows
            == if archived {
                expected.rows + u64::try_from(served.len())?
            } else {
                0
            }
    );
    for expected in served {
        let value = receipts::lookup_candidate(
            &tx,
            &h.fixture.installation,
            &h.fixture.tenant,
            &expected.receipt.execution_id,
        )
        .await?
        .context("actual served receipt lost")?;
        ensure!(
            value.digest == expected.digest
                && value.bytes() == expected.bytes()
                && value.auxiliary_bytes() == expected.auxiliary_bytes()
        );
    }
    unknown.verify(&tx, h).await?;
    tx.rollback().await?;
    eprintln!(
        "canonical history rows={} payload_bytes={} digest={} archived={} unknown_count={}",
        actual.rows,
        actual.payload,
        actual.digest(),
        archived,
        inventory.unknown_count
    );
    Ok(())
}

async fn archive_all(h: &Harness, rows: u64) -> Result<()> {
    let started = std::time::Instant::now();
    // Fewer than 32 unknown sentinels cannot hide all eligible keys. False alone
    // is insufficient: the final exact archive count is the completion witness.
    for _ in 0..=rows {
        if !h
            .fixture
            .service
            .archive_history_for_test()
            .await
            .map_err(failure)?
        {
            break;
        }
    }
    let row = h
        .fixture
        .db
        .query_one(sql("SELECT COUNT(*) AS count FROM tenancy_receipt_archive"))
        .await?
        .context("archive count")?;
    ensure!(u64::try_from(row.try_get::<i64>("", "count")?)? == rows);
    ensure!(
        !h.fixture
            .service
            .archive_history_for_test()
            .await
            .map_err(failure)?
    );
    eprintln!(
        "history archive rows={rows} elapsed={:?}",
        started.elapsed()
    );
    Ok(())
}

async fn late_reads(
    h: &Harness,
    ledger: &Ledger,
    reference: &ResourceRef,
    token: &str,
) -> Result<()> {
    for index in [0, ledger.rows / 2, ledger.rows - 1] {
        let path = format!(
            "/control/v1/tenants/{}/executions/{}",
            h.fixture.tenant,
            id(index)
        );
        let (status, body) = h.wire(Method::GET, &path, token, &()).await?;
        ensure!(status == StatusCode::OK);
        let receipt: ExecutionReceipt = serde_json::from_slice(&body)?;
        ensure!(receipt.execution_id.as_str() == id(index));
        let expected = candidate(h, reference, &id(index))?;
        ensure!(serde_json::to_vec(&receipt)? == serde_json::to_vec(&expected.receipt)?);
        let (status, body) = h
            .wire(Method::GET, &format!("{path}/usage-evidence"), token, &())
            .await?;
        ensure!(status == StatusCode::OK);
        let evidence: bitrouter_ai::tenancy::usage_evidence::ExecutionUsageEvidenceV1 =
            serde_json::from_slice(&body)?;
        ensure!(evidence.execution_id == receipt.execution_id);
        bitrouter_ai::tenancy::validation::Validate::validate(&evidence)?;
        // The protected reader authenticates the complete immutable sidecar;
        // compare its actual length and every attempt binding with the fixture.
        ensure!(u64::try_from(body.len())? == usage_bytes(&expected)?);
        ensure!(!evidence.frames.is_empty() && evidence.attempts.len() == receipt.attempts.len());
        for (observed, attempt) in evidence.attempts.iter().zip(&receipt.attempts) {
            ensure!(
                observed.attempt_id == attempt.attempt_id
                    && observed.outcome == attempt.outcome
                    && serde_json::to_vec(&observed.target)?
                        == serde_json::to_vec(&attempt.target)?
            );
        }
    }
    let path = format!(
        "/control/v1/tenants/{}/executions?correlation_id=history-correlation&limit=3",
        h.fixture.tenant
    );
    let (status, body) = h.wire(Method::GET, &path, token, &()).await?;
    ensure!(status == StatusCode::OK);
    let page: bitrouter_ai::tenancy::execution::ExecutionPage = serde_json::from_slice(&body)?;
    ensure!(page.execution_ids.len() == 3 && page.next_after.is_some());
    Ok(())
}

pub(super) async fn exercise(
    h: &mut Harness,
    retained: &authority::Retained,
    large: bool,
) -> Result<()> {
    let serving = serving::Serving::new(h, &retained.head).await?;
    let mut served = Vec::new();
    let reader = h.fixture.issue("history-reader").await?;
    let token = reader.secret.as_deref().context("history reader secret")?;
    h.fixture.service.shutdown().await?;
    let unknown = unknown::seed(h, &retained.head).await?;
    let mut ledger = Ledger::default();
    let mut before = disk::checkpoint(h).await?;
    let mut cold_growth = 0;
    for cycle in 0..3 {
        let payload_before = ledger.payload;
        append(
            h,
            &retained.head,
            &mut ledger,
            if large {
                if cycle == 0 { 8 * GIB + GIB / 4 } else { GIB }
            } else {
                0
            },
        )
        .await?;
        let inserted = disk::checkpoint(h).await?;
        if cycle == 0 {
            cold_growth = inserted.pages.saturating_sub(before.pages);
            verify(h, &ledger, &unknown, &served, false).await?;
            if large {
                ensure!(ledger.payload > 8 * GIB && inserted.pages * inserted.page_size > 8 * GIB);
            }
            let row = h.fixture.db.query_one(sql("SELECT SUM(length(CAST(r.receipt_json AS BLOB)) + length(CAST(u.evidence_json AS BLOB))) AS payload FROM tenancy_execution_receipts r JOIN tenancy_usage_evidence u USING (installation_id,tenant_id,execution_id) WHERE r.execution_id LIKE 'history-%' AND r.storage_kind=0"))
                .await?.context("actual inline lengths")?;
            ensure!(u64::try_from(row.try_get::<i64>("", "payload")?)? == ledger.payload);
        } else {
            disk::reused(&before, &inserted, ledger.payload - payload_before)?;
        }
        h.restart().await?;
        archive_all(h, ledger.rows + u64::try_from(served.len())?).await?;
        served.push(serving.send(h).await?);
        archive_all(h, ledger.rows + u64::try_from(served.len())?).await?;
        late_reads(h, &ledger, &retained.head, token).await?;
        authority::verify(h, retained).await?;
        h.fresh_mutation(&format!("history-cycle-{cycle}")).await?;
        h.fixture.service.shutdown().await?;
        let archived = disk::checkpoint(h).await?;
        ensure!(archived.free > inserted.free);
        if cycle == 0 {
            ensure!(archived.free - inserted.free > cold_growth / 2);
        }
        verify(h, &ledger, &unknown, &served, true).await?;
        eprintln!("history reuse cycle={cycle} inserted={inserted:?} archived={archived:?}");
        before = archived;
    }
    // Duplicate completion after relocation remains an exact no-op, not inline resurrection.
    let tx = h.fixture.db.begin().await?;
    for index in [0, ledger.rows / 2, ledger.rows - 1] {
        ensure!(!receipts::insert(&tx, &candidate(h, &retained.head, &id(index))?).await?);
    }
    tx.commit().await?;
    h.restart().await?;
    late_reads(h, &ledger, &retained.head, token).await?;
    let revoke = Mutation {
        operation_id: "history-revoke-reader".try_into()?,
        expected_revision: reader.credential_revision,
        installation_id: h.fixture.installation.clone(),
    };
    let applied: OperationResult = h
        .api(
            Method::DELETE,
            &format!(
                "/control/v1/tenants/{}/credentials/{}",
                h.fixture.tenant, reader.credential_id
            ),
            &revoke,
        )
        .await?;
    ensure!(applied.status == OperationStatus::Applied);
    let (status, _) = h
        .wire(
            Method::GET,
            &format!(
                "/control/v1/tenants/{}/executions/{}",
                h.fixture.tenant,
                id(0)
            ),
            token,
            &(),
        )
        .await?;
    ensure!(status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN);
    late_reads(h, &ledger, &retained.head, TOKEN).await?;
    if !large {
        let default_unknown = h.config.capacity.receipts.unknown_tenant;
        h.config.capacity.receipts.unknown_tenant = 2;
        h.restart().await?;
        let pin = h
            .fixture
            .service
            .pin_unknown_count(
                &h.fixture.tenant,
                tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            )
            .await
            .map_err(failure)?;
        ensure!(matches!(
            h.fixture
                .service
                .admission
                .acquire(
                    &h.fixture.tenant,
                    &bitrouter_ai::tenancy::configuration::Limits {
                        active_executions: 1,
                        queued_work: 0,
                        learning_jobs: 0
                    }
                )
                .await,
            Err(ErrorCode::CapacityExceeded)
        ));
        h.config.capacity.receipts.unknown_tenant = default_unknown;
        drop(pin);
        h.restart().await?;
    }
    h.fixture.service.shutdown().await?;
    verify(h, &ledger, &unknown, &served, true).await?;
    h.restart().await?;
    authority::verify(h, retained).await?;
    late_reads(h, &ledger, &retained.head, TOKEN).await?;
    h.fresh_mutation("history-final-fresh").await?;
    served.push(serving.send(h).await?);
    ensure!(served.len() == 4);
    archive_all(h, ledger.rows + u64::try_from(served.len())?).await?;
    h.fixture.service.shutdown().await?;
    verify(h, &ledger, &unknown, &served, true).await?;
    if large {
        ensure!(ledger.payload >= 10 * GIB + GIB / 4);
        ensure!(std::fs::metadata(&h.path)?.len() > 8 * GIB);
    }
    // The same original file, including its released SQLite pages, crosses the
    // supported offline APIs. Keep its fence while the restored target serves.
    let original = offline::roundtrip(h, !large).await?;
    authority::verify(h, retained).await?;
    late_reads(h, &ledger, &retained.head, TOKEN).await?;
    let (status, _) = h
        .wire(
            Method::GET,
            &format!(
                "/control/v1/tenants/{}/executions/{}",
                h.fixture.tenant,
                id(0)
            ),
            token,
            &(),
        )
        .await?;
    ensure!(status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN);
    h.fresh_mutation("history-restored-fresh").await?;
    served.push(serving.send(h).await?);
    ensure!(served.len() == 5);
    archive_all(h, ledger.rows + u64::try_from(served.len())?).await?;
    h.fixture.service.shutdown().await?;
    verify(h, &ledger, &unknown, &served, true).await?;
    original.unchanged().await?;
    eprintln!(
        "history actual_provider_requests=5 successful=5 refused=0 failures=0 (any unexpected result fails test); restored canonical history/unknown/authentication verified; external_state_recovery_qualified=false"
    );
    Ok(())
}
