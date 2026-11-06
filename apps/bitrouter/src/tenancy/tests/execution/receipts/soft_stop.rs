//! A stopped waiter cannot grant a retry or weaken an independent storage fence.

use super::*;
use crate::tenancy::{
    storage::Budget,
    transaction::{Control, Proof},
};
use bitrouter_ai::tenancy::{execution::Outcome, operation::ErrorCode};
use futures::FutureExt;
use std::sync::atomic::AtomicBool;

fn code(error: ErrorCode) -> anyhow::Error {
    anyhow::anyhow!("receipt test failed: {error:?}")
}

#[cfg(all(
    unix,
    feature = "core",
    feature = "harness",
    any(feature = "otel-http", feature = "otel-grpc")
))]
#[tokio::test]
async fn ordinary_native_refusal_preserves_actual_gateway_receipt_maintenance() -> Result<()> {
    use crate::harness::storage::{ExecutionRecord, ExecutionStore};
    use crate::tenancy::transaction::Phase;
    let provider = Arc::new(Semaphore::new(0));
    let mut h = Harness::with_reply(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
        Some(provider.clone()),
    )
    .await?;
    let budget = h.fixture.service.storage_budget();
    let native = crate::agent_store::DatabaseExecutionStore::new(h.fixture.db.clone())
        .with_storage_budget(Some(budget.clone()));
    let claim = native
        .claim_owner("native-pressure")
        .await
        .map_err(anyhow::Error::msg)?;
    let bitrouter_ai::harness::runtime::OwnerClaim::Acquired { owner } = claim else {
        anyhow::bail!("native owner not acquired");
    };
    let client = Harness::client(false)?;
    let request = h.send(&client);
    let caller = AbortOnDropHandle::new(tokio::spawn(async move {
        let response = request.send().await?;
        ensure!(response.status() == StatusCode::OK);
        let id = execution_id(&response)?;
        ensure!(!response.bytes().await?.is_empty());
        Ok::<_, anyhow::Error>(id)
    }));
    tokio::time::timeout(TIMEOUT, h.received.recv())
        .await?
        .context("actual provider request")?;
    let before = budget
        .calibration_observation(None)
        .map_err(|e| anyhow::anyhow!("receipt observation: {e:?}"))?;
    ensure!(before.observed_free.is_none() && before.completion_reserved > 0);
    // Below ordinary's default 768 MiB floor, with room for the existing
    // core+harness receipt allowance and this admitted completion obligation.
    h.fixture.service.admission.receipt_probe(512 * 1024 * 1024);
    let refused = native
        .commit_owned(&owner, "native-new", 0, &[ExecutionRecord::CoreDispatch])
        .await;
    ensure!(refused.is_err_and(|error| error.contains("CapacityExceeded")));
    let after = budget
        .calibration_observation(None)
        .map_err(|e| anyhow::anyhow!("native refusal observation: {e:?}"))?;
    ensure!(
        !after.fenced
            && after.physical_write_reserved == 0
            && after.completion_reserved == before.completion_reserved
    );
    let gate = h.fixture.service.cleanup.hold(Phase::Committing);
    provider.add_permits(1);
    let held: Result<()> = async {
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        let pending = budget
            .calibration_observation(None)
            .map_err(|e| anyhow::anyhow!("actual receipt reservation: {e:?}"))?;
        ensure!(!pending.fenced && pending.physical_write_reserved > 0);
        ensure!(pending.completion_reserved == before.completion_reserved);
        Ok(())
    }
    .await;
    h.fixture.service.cleanup.release_gate();
    held?;
    let id = tokio::time::timeout(TIMEOUT, caller).await???;
    tokio::time::timeout(TIMEOUT, h.fixture.service.admission.wait_drained()).await?;
    let receipt = store::lookup_candidate(
        &h.fixture.db,
        &h.fixture.installation,
        &h.fixture.tenant,
        &id,
    )
    .await?
    .context("actual completed receipt")?;
    ensure!(receipt.receipt.outcome == Outcome::Completed);
    ensure!(
        budget
            .calibration_observation(None)
            .map_err(|e| anyhow::anyhow!("settled receipt observation: {e:?}"))?
            .physical_write_reserved
            == 0
    );
    ensure!(
        native
            .stop_owner(&owner)
            .await
            .map_err(anyhow::Error::msg)?
            .stopped_at_ms
            .is_some()
    );
    tokio::time::timeout(TIMEOUT, h.fixture.service.shutdown()).await??;
    Ok(())
}

#[cfg(all(unix, any(feature = "otel-http", feature = "otel-grpc")))]
#[tokio::test]
async fn actual_readonly_receipt_fault_stays_fenced_after_acknowledged_rollback() -> Result<()> {
    use crate::tenancy::transaction::{Disposition, Phase, receipt_observation::Key};
    use sea_orm::ConnectionTrait;
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    let execution = super::super::admitted(&harness.fixture, &harness.headers).await?;
    let owner = execution.receipt_owner()?.clone();
    let proof = &harness.fixture.service.cleanup;
    proof.observe_receipt_for_test(Key {
        installation: harness.fixture.installation.clone(),
        tenant: harness.fixture.tenant.clone(),
        execution: owner.id().clone(),
    })?;
    let budget = harness.fixture.service.storage_budget();
    let observe = || {
        budget
            .calibration_observation(None)
            .map_err(|error| anyhow::anyhow!("receipt capacity: {error:?}"))
    };
    let before = observe()?;
    ensure!(
        !before.fenced && before.physical_write_reserved == 0 && before.completion_reserved > 0
    );
    // SQLite returns typed READONLY for the actual receipt INSERT, while the
    // actual transaction can still acknowledge ROLLBACK. No fabricated outcome.
    harness
        .fixture
        .db
        .execute_unprepared("PRAGMA query_only=ON")
        .await?;
    let result: Result<()> = async {
        let gate = proof.hold(Phase::RollingBack);
        let completing = owner.clone();
        let finish = AbortOnDropHandle::new(tokio::spawn(async move {
            completing.finish(Outcome::Cancelled).await
        }));
        let held: Result<()> = async {
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            ensure!(proof.disposition() == Disposition::Pending);
            let current = observe()?;
            ensure!(current.fenced && current.physical_write_reserved > 0);
            ensure!(current.completion_reserved == before.completion_reserved);
            for maintenance in [false, true] {
                ensure!(matches!(
                    budget.reserve(0, maintenance),
                    Err(ErrorCode::StorageUnavailable)
                ));
            }
            Ok(())
        }
        .await;
        proof.release_gate();
        let finished = tokio::time::timeout(TIMEOUT, finish).await??;
        held?;
        ensure!(finished == Err(ErrorCode::StorageUnavailable));
        ensure!(proof.disposition() == Disposition::RolledBack);
        let trace = proof.receipt_trace_for_test()?;
        ensure!(!trace.overflow && trace.facts[1].is_none());
        let fact = trace.facts[0].as_ref().context("actual receipt attempt")?;
        ensure!(
            fact.store
                .is_some_and(|outcome| !outcome.ok && outcome.sqlite_code == Some(8))
        );
        ensure!(fact.rollback.is_some_and(|outcome| outcome.ok));
        ensure!(fact.commit.is_none() && !owner.durable());
        let settled = observe()?;
        ensure!(settled.fenced && settled.physical_write_reserved == 0);
        ensure!(settled.completion_reserved == before.completion_reserved);
        harness
            .fixture
            .db
            .execute_unprepared("PRAGMA query_only=OFF")
            .await?;
        // The fixture has an unbound volume. A healthy later space observation
        // cannot clear the process storage-health fence or admit maintenance.
        harness.fixture.service.admission.receipt_probe(u64::MAX);
        for maintenance in [false, true] {
            ensure!(matches!(
                budget.reserve(0, maintenance),
                Err(ErrorCode::StorageUnavailable)
            ));
        }
        ensure!(harness.calls.load(Ordering::SeqCst) == 0);
        ensure!(
            store::lookup(
                &harness.fixture.db,
                &harness.fixture.installation,
                &harness.fixture.tenant,
                owner.id()
            )
            .await?
            .is_none()
        );
        Ok(())
    }
    .await;
    proof.release_gate();
    let restored = harness
        .fixture
        .db
        .execute_unprepared("PRAGMA query_only=OFF")
        .await;
    drop(execution);
    let stopped = tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await;
    result?;
    restored?;
    ensure!(stopped?.is_err());
    Ok(())
}

#[tokio::test]
async fn failed_before_prepare_cannot_be_promoted_by_no_transaction_failure() -> Result<()> {
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    let execution = super::super::admitted(&harness.fixture, &harness.headers).await?;
    execution.revalidate().map_err(code)?;
    let owner = execution.receipt_owner()?.clone();
    let admission = &harness.fixture.service.admission;
    let sql_before = harness.fixture.service.cleanup.sql_counts_for_test();

    // Poll just the waiter on this current-thread runtime: enqueue the first
    // proposal, then fail its owner before the writer can prepare that job.
    let finish = owner.finish(Outcome::Cancelled);
    tokio::pin!(finish);
    ensure!(finish.as_mut().now_or_never().is_none());
    owner.fail(ErrorCode::StorageUnavailable);
    ensure!(owner.prepare().await.is_err());
    // This is the actor's classification after the failed preparation. It must
    // not turn the earlier nonretryable failure into a fresh terminal attempt.
    owner.storage_failed(true);
    owner.reconcile().map_err(code)?;
    ensure!(owner.failure() == Some(ErrorCode::StorageUnavailable));
    ensure!(owner.prepare().await.is_err());
    ensure!(!owner.durable());
    ensure!(finish.await == Err(ErrorCode::StorageUnavailable));
    ensure!(harness.fixture.service.cleanup.sql_counts_for_test() == sql_before);

    // Soft stop denies admission/sends, while trusted maintenance accounting
    // remains usable. Neither property proves that this failed owner is clean.
    ensure!(!admission.receipts_ready());
    ensure!(execution.revalidate().is_err());
    // Admitted delivery authority survives a soft stop; the failed terminal
    // waiter above still cannot report a durable receipt.
    execution.check_delivery().map_err(code)?;
    ensure!(admission.unknown_read(false).is_err());
    drop(admission.unknown_read(true).map_err(code)?);
    Budget::new(admission.clone(), harness.fixture.service.cleanup.clone())
        .reserve(0, true)
        .map_err(code)?
        .complete()
        .map_err(code)?;
    ensure!(admission.wait_drained().now_or_never().is_none());
    ensure!(harness.calls.load(Ordering::SeqCst) == 0);
    drop(execution);
    ensure!(
        tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown())
            .await?
            .is_err()
    );
    ensure!(owner.failure() == Some(ErrorCode::StorageUnavailable));
    ensure!(!owner.durable());
    ensure!(
        store::lookup(
            &harness.fixture.db,
            &harness.fixture.installation,
            &harness.fixture.tenant,
            owner.id(),
        )
        .await?
        .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn hard_accounting_fence_dominates_owner_soft_stop_in_either_order() -> Result<()> {
    for hard_first in [true, false] {
        let harness = Harness::new(
            "chat_completions",
            StatusCode::OK,
            json_response("chat_completions", full_usage("chat_completions"))?,
        )
        .await?;
        let execution = super::super::admitted(&harness.fixture, &harness.headers).await?;
        let owner = execution.receipt_owner()?.clone();
        let admission = &harness.fixture.service.admission;
        let before_fence = admission.unknown_read(true).map_err(code)?;

        if hard_first {
            admission.fence_receipts();
        }
        owner.fail(ErrorCode::StorageUnavailable);
        if !hard_first {
            admission.fence_receipts();
        }
        owner.storage_failed(true);
        owner.reconcile().map_err(code)?;
        ensure!(owner.failure() == Some(ErrorCode::StorageUnavailable));
        ensure!(!admission.receipts_ready());
        ensure!(admission.unknown_read(true).is_err());
        ensure!(matches!(
            Budget::new(admission.clone(), harness.fixture.service.cleanup.clone())
                .reserve(0, true),
            Err(ErrorCode::StorageUnavailable)
        ));

        // Even an earlier valid read plus an actual pending transaction cannot
        // bypass the hard accounting fence at publication preparation.
        let db = crate::db::connect("sqlite::memory:").await?;
        let proof = Proof::new(Arc::new(AtomicBool::new(true)));
        let control = Control::new(proof.clone());
        let transaction = control.begin(&db).await?;
        let publication = admission.prepare_unknown(&before_fence, Vec::new(), Vec::new(), proof);
        control.rollback(transaction).await?;
        ensure!(matches!(publication, Err(ErrorCode::StorageUnavailable)));
        db.close().await?;
        ensure!(admission.wait_drained().now_or_never().is_none());
        ensure!(harness.calls.load(Ordering::SeqCst) == 0);
        drop(execution);
        ensure!(
            tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown())
                .await?
                .is_err()
        );
    }
    Ok(())
}
