//! A final database refusal retains the real writer's original receipt owner.
#![cfg(unix)]

use super::*;
use crate::tenancy::transaction::{Disposition, Phase, receipt_observation::Key};
use bitrouter_ai::tenancy::{execution::Outcome, operation::ErrorCode};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

#[tokio::test]
async fn final_database_refusal_rolls_back_staged_receipt_and_retains_original_owner() -> Result<()>
{
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    // Complete admission reads before installing the transaction gate.
    let execution = super::super::admitted(&harness.fixture, &harness.headers).await?;
    let owner = execution.receipt_owner()?.clone();
    let service = &harness.fixture.service;
    let proof = &service.cleanup;
    proof.observe_receipt_for_test(Key {
        installation: harness.fixture.installation.clone(),
        tenant: harness.fixture.tenant.clone(),
        execution: owner.id().clone(),
    })?;
    let reserved = || {
        service
            .admission
            .receipt_capacity_for_test()
            .map(|value| value.0)
            .map_err(|error| anyhow::anyhow!("receipt reservation: {error:?}"))
    };
    let before = reserved()?;
    ensure!(before > 0);
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    let budget = service.storage_budget();
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    let physical = || {
        budget
            .calibration_observation(None)
            .map(|value| value.physical_write_reserved)
            .map_err(|error| anyhow::anyhow!("physical reservation: {error:?}"))
    };
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    ensure!(physical()? == 0);

    // Mutate the indexed receipt only after both INSERTs really ran. Staging
    // succeeds; the final canonical read must reject this SQL projection.
    // https://www.sqlite.org/lang_createtrigger.html
    harness
        .fixture
        .db
        .execute_unprepared(
            "CREATE TRIGGER receipt_terminal_projection AFTER INSERT ON tenancy_usage_evidence \
         BEGIN UPDATE tenancy_execution_receipts SET principal_id='foreign' \
         WHERE installation_id=NEW.installation_id AND tenant_id=NEW.tenant_id \
         AND execution_id=NEW.execution_id; END",
        )
        .await?;
    let result: Result<()> = async {
        let gate = proof.hold(Phase::RollingBack);
        let completing = owner.clone();
        let finish = AbortOnDropHandle::new(tokio::spawn(async move {
            completing.finish(Outcome::Cancelled).await
        }));
        let held: Result<()> = async {
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            ensure!(proof.disposition() == Disposition::Pending && !owner.durable());
            ensure!(reserved()? == before);
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            ensure!(physical()? > 0);
            let trace = proof.receipt_trace_for_test()?;
            let fact = trace.facts[0].as_ref().context("staged receipt at rollback gate")?;
            ensure!(fact.store.is_some_and(|outcome| outcome.ok) && fact.inserted == Some(true));
            ensure!(fact.rollback.is_none() && fact.commit.is_none());
            Ok(())
        }.await;
        proof.release_gate();
        let finished = tokio::time::timeout(TIMEOUT, finish).await??;
        held?;
        ensure!(finished == Err(ErrorCode::StorageUnavailable));
        ensure!(proof.disposition() == Disposition::RolledBack && !owner.durable());
        let trace = proof.receipt_trace_for_test()?;
        ensure!(!trace.overflow && trace.facts[1].is_none());
        let fact = trace.facts[0].as_ref().context("actual refused receipt")?;
        ensure!(fact.begin.is_some_and(|outcome| outcome.ok));
        ensure!(fact.store.is_some_and(|outcome| outcome.ok) && fact.inserted == Some(true));
        ensure!(fact.rollback.is_some_and(|outcome| outcome.ok) && fact.commit.is_none());
        ensure!(reserved()? == before && !service.admission.receipts_ready());
        #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
        ensure!(physical()? == 0);
        for table in [
            "tenancy_execution_receipts", "tenancy_usage_evidence",
            "tenancy_receipt_auxiliary", "tenancy_unknown_obligations",
        ] {
            let row = harness.fixture.db.query_one(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                format!("SELECT COUNT(*) AS total FROM {table} WHERE installation_id=? AND tenant_id=? AND execution_id=?"),
                [harness.fixture.installation.as_str().into(), harness.fixture.tenant.as_str().into(), owner.id().as_str().into()],
            )).await?.context("actual rollback row count")?;
            ensure!(row.try_get::<i64>("", "total")? == 0, "rollback retained {table}");
        }
        ensure!(harness.calls.load(Ordering::SeqCst) == 0);
        Ok(())
    }.await;
    // Cleanup is attempted even when a gate/SQL assertion fails, and its
    // failure cannot turn an unsuccessful test into a pass.
    proof.release_gate();
    let restored = harness
        .fixture
        .db
        .execute_unprepared("DROP TRIGGER receipt_terminal_projection")
        .await;
    drop(execution);
    let stopped = tokio::time::timeout(TIMEOUT, service.shutdown()).await;
    result?;
    restored?;
    stopped??;

    let trace = proof.receipt_trace_for_test()?;
    ensure!(!trace.overflow);
    let first = trace.facts[0]
        .as_ref()
        .context("original rollback attempt")?;
    let second = trace.facts[1]
        .as_ref()
        .context("original owner reconciliation")?;
    ensure!(first.fingerprint == second.fingerprint);
    ensure!(second.begin.is_some_and(|outcome| outcome.ok));
    ensure!(second.store.is_some_and(|outcome| outcome.ok) && second.inserted == Some(true));
    ensure!(second.commit.is_some_and(|outcome| outcome.ok) && second.rollback.is_none());
    ensure!(second.disposition == Disposition::Committed && owner.durable());
    ensure!(reserved()? == 0 && !service.admission.receipts_ready());
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    ensure!(physical()? == 0);
    let actual = store::lookup_candidate(
        &harness.fixture.db,
        &harness.fixture.installation,
        &harness.fixture.tenant,
        owner.id(),
    )
    .await?
    .context("canonical reconciled receipt")?;
    ensure!(actual.receipt.outcome == Outcome::Cancelled && actual.receipt.attempts.is_empty());
    ensure!(harness.calls.load(Ordering::SeqCst) == 0);
    Ok(())
}
