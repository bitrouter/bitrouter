//! One real receipt pair and its same-actor archive writes, with sampled files.
use super::*;
use crate::{storage::write::measurement, tenancy::transaction::Phase};
use bitrouter_ai::tenancy::{execution::Outcome, usage_evidence::ExecutionUsageEvidenceV1};
use sea_orm::{ConnectionTrait, Statement};

fn usage_stream() -> Result<String> {
    let mut body = String::new();
    for n in 1..=16 {
        let chunk = json!({
            "id":"chatcmpl-receipts", "object":"chat.completion.chunk", "model":"model-a",
            "choices":[{"index":0,"delta":if n == 1 { json!({"role":"assistant","content":"reply"}) } else { json!({"content":"reply"}) },"finish_reason":null}],
            "usage":{"prompt_tokens":11,"completion_tokens":n,"total_tokens":11+n}
        });
        body.push_str(&format!("data: {}\n\n", serde_json::to_string(&chunk)?));
    }
    let mut terminal = full_usage("chat_completions");
    terminal["completion_tokens"] = json!(17);
    terminal["total_tokens"] = json!(28);
    let terminal = json!({
        "id":"chatcmpl-receipts", "object":"chat.completion.chunk", "model":"model-a",
        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":terminal
    });
    body.push_str(&format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::to_string(&terminal)?
    ));
    Ok(body)
}

async fn evidence(h: &Harness, id: &ExecutionId) -> Result<Vec<u8>> {
    h.fixture
        .service
        .read_receipt(
            h.fixture.operator.clone(),
            h.fixture.tenant.clone(),
            read::Request::UsageEvidence(id.clone()),
        )
        .await
        .map_err(failure)
}

#[tokio::test]
async fn actual_receipt_and_same_actor_archive_estimates_cover_sampled_file_growth() -> Result<()> {
    let mut receipts = Vec::new();
    let mut archives = Vec::new();
    for stream in [false, true] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("receipt-calibration.sqlite");
        let body = if stream {
            usage_stream()?
        } else {
            serde_json::to_string(&json_response(
                "chat_completions",
                full_usage("chat_completions"),
            )?)?
        };
        let provider = Arc::new(Semaphore::new(0));
        let mut h = Harness::with_observed_storage(
            "chat_completions",
            StatusCode::OK,
            body,
            stream,
            Some(provider.clone()),
            false,
            (
                crate::telemetry::receipt_latency::Sink::default(),
                Some(&path),
            ),
        )
        .await?;
        let db = h.fixture.db.clone();
        let result: Result<()> = async {
            let budget = h.fixture.service.storage_budget();
            let client = Harness::client(false)?;
            let request = h.send(&client);
            let caller = AbortOnDropHandle::new(tokio::spawn(async move {
                let response = request.send().await?;
                ensure!(response.status() == StatusCode::OK);
                let id = execution_id(&response)?;
                ensure!(!response.bytes().await?.is_empty());
                Ok::<_, anyhow::Error>(id)
            }));
            tokio::time::timeout(TIMEOUT, h.received.recv()).await?.context("actual provider request")?;
            ensure!(measurement::reservation(&budget)? == 0);
            // The provider still owns its response. No receipt SQL can race
            // baseline checkpointing; after release, only this owner completes.
            let before = measurement::wal_baseline(&db, &path).await?;
            let gate = h.fixture.service.cleanup.hold(Phase::Committing);
            provider.add_permits(1);
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            let charged = measurement::reservation(&budget)?;
            let pending = measurement::files(&path)?;
            ensure!(!caller.is_finished());
            h.fixture.service.cleanup.release_gate();
            let id = tokio::time::timeout(TIMEOUT, caller).await???;
            tokio::time::timeout(TIMEOUT, h.fixture.service.admission.wait_drained()).await?;
            ensure!(measurement::reservation(&budget)? == 0);
            let encoded = measurement::report(&db, &path, &format!("receipt/stream={stream}"), &before, pending, charged).await?;
            let candidate = store::lookup_candidate(&db, &h.fixture.installation, &h.fixture.tenant, &id)
                .await?.context("completed receipt")?;
            ensure!(candidate.receipt.outcome == Outcome::Completed);
            let actual = candidate.bytes().checked_add(candidate.auxiliary_bytes()).context("candidate bytes")?;
            let expected = if cfg!(all(feature = "core", feature = "harness")) {
                actual.max(u64::try_from(crate::storage::operation_budget::MAX_BYTES)?)
            } else { actual };
            ensure!(encoded == expected);
            let usage = evidence(&h, &id).await?;
            let document: ExecutionUsageEvidenceV1 = serde_json::from_slice(&usage)?;
            ensure!(document.frames.len() == if stream { 17 } else { 1 });
            receipts.push((actual, encoded));

            let prepared = store::archive::Prepared::new(&candidate, &|_, _| Ok(()))?;
            let expected = u64::try_from(prepared.stored_bytes()
                .checked_add(prepared.relocated_bytes()).and_then(|n| n.checked_add(16 * 1024))
                .context("archive encoded bytes")?)?;
            let logical = budget.calibration_observation(None)
                .map_err(|e| anyhow::anyhow!("archive accounting: {e:?}"))?;
            ensure!(logical.active == 0 && logical.physical_write_reserved == 0);
            let before = measurement::wal_baseline(&db, &path).await?;
            let gate = h.fixture.service.cleanup.hold(Phase::Committing);
            let service = h.fixture.service.clone();
            let archiver = AbortOnDropHandle::new(tokio::spawn(async move {
                service.archive_history_for_test().await.map_err(failure)
            }));
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            let held = budget.calibration_observation(None)
                .map_err(|e| anyhow::anyhow!("held archive accounting: {e:?}"))?;
            let pending = measurement::files(&path)?;
            ensure!(held.accounted == logical.accounted
                && held.history_bytes == logical.history_bytes && held.history_rows == logical.history_rows);
            h.fixture.service.cleanup.release_gate();
            ensure!(tokio::time::timeout(TIMEOUT, archiver).await???);
            ensure!(measurement::reservation(&budget)? == 0);
            let encoded = measurement::report(&db, &path, &format!("archive/stream={stream}"), &before, pending, held.physical_write_reserved).await?;
            ensure!(encoded == expected);
            archives.push(encoded);
            let settled = budget.calibration_observation(None)
                .map_err(|e| anyhow::anyhow!("settled archive accounting: {e:?}"))?;
            ensure!(settled.accounted.checked_add(prepared.source_bytes()) == Some(logical.accounted));
            ensure!(logical.history_bytes.checked_add(prepared.history_bytes()) == Some(settled.history_bytes));
            ensure!(settled.history_rows == logical.history_rows + 1);
            let archived = store::lookup_candidate(&db, &h.fixture.installation, &h.fixture.tenant, &id)
                .await?.context("archived receipt")?;
            ensure!(archived.digest == candidate.digest
                && serde_json::to_vec(&archived.receipt)? == serde_json::to_vec(&candidate.receipt)?);
            ensure!(evidence(&h, &id).await? == usage);
            let row = db.query_one(Statement::from_sql_and_values(db.get_database_backend(),
                "SELECT codec FROM tenancy_receipt_archive WHERE installation_id=? AND tenant_id=? AND execution_id=?",
                [h.fixture.installation.as_str().into(), h.fixture.tenant.as_str().into(), id.as_str().into()]))
                .await?.context("actual archive codec")?;
            let codec: i64 = row.try_get("", "codec")?;
            ensure!(matches!(codec, 0 | 1));
            eprintln!("caller calibration archive stream={stream} codec={codec} stored={} relocated={}", prepared.stored_bytes(), prepared.relocated_bytes());
            ensure!(h.calls.load(Ordering::SeqCst) == 1);
            Ok(())
        }.await;
        h.fixture.service.cleanup.release_gate();
        provider.add_permits(1);
        let stopped = h.fixture.service.shutdown().await;
        h._gateway.abort();
        h._provider.abort();
        let _ = (&mut h._gateway).await;
        let _ = (&mut h._provider).await;
        drop(h);
        let closed = db.close().await;
        let removed = directory.close();
        result?;
        stopped?;
        closed?;
        removed?;
    }
    ensure!(receipts.len() == 2 && receipts[1].0 > receipts[0].0);
    if !cfg!(all(feature = "core", feature = "harness")) {
        ensure!(receipts[1].1 > receipts[0].1);
    }
    ensure!(archives.len() == 2 && archives[1] > archives[0]);
    Ok(())
}
