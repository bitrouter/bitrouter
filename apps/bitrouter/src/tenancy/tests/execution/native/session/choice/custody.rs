use super::*;
use crate::tenancy::transaction::{Disposition, Phase, receipt_observation::Key};

fn observe_child(bound: &Bound, child: &str) -> Result<()> {
    bound.fixture.service.cleanup.observe_receipt_for_test(Key {
        installation: bound.fixture.installation.clone(),
        tenant: bound.fixture.tenant.clone(),
        execution: child.try_into()?,
    })
}

fn receipt_committed(bound: &Bound) -> Result<()> {
    let trace = bound.fixture.service.cleanup.receipt_trace_for_test()?;
    ensure!(!trace.overflow);
    let fact = trace.facts[0]
        .as_ref()
        .context("actual child receipt SQL")?;
    ensure!(fact.store.is_some_and(|result| result.ok) && fact.inserted == Some(true));
    ensure!(fact.commit.is_some_and(|result| result.ok) && fact.fingerprint.attempt_count == 1);
    Ok(())
}

#[tokio::test]
async fn owned_choice_cancellation_joins_child_receipt_and_core_outcome_as_distinct_acks()
-> Result<()> {
    let generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::start(true, false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let bound = fixture::sqlite(&generation, &choice, directory.path(), 2).await?;
    let checked = tokio::time::timeout(Duration::from_secs(25), async {
        let mut session = fixture::open(
            &bound,
            &generation,
            &workspace,
            Arc::new(HostSpies::default()),
        )
        .await?;
        let core = session.core().clone();
        let running = AbortOnDropHandle::new(tokio::spawn(async move {
            session
                .run(
                    fixture::task(),
                    Arc::new(Approval),
                    CancellationToken::new(),
                )
                .await
        }));
        choice.receive().await?;
        let saved = fixture::journal(&bound).await?;
        let (id, link) = saved["owned_choices"]
            .as_object()
            .context("intent")?
            .iter()
            .next()
            .context("intent link")?;
        let child = link["child_execution"].as_str().context("child")?;
        observe_child(&bound, child)?;
        let receipt_gate = bound.fixture.service.cleanup.hold(Phase::Committing);
        choice.release();
        tokio::time::timeout(TIMEOUT, receipt_gate.entered.notified()).await?;
        let trace = bound.fixture.service.cleanup.receipt_trace_for_test()?;
        let fact = trace.facts[0].as_ref().context("held child receipt SQL")?;
        ensure!(fact.store.is_some_and(|value| value.ok) && fact.commit.is_none());
        ensure!(
            core.snapshot().await.context_store.decisions[id]
                .outcome
                .is_none()
        );
        ensure!(bound.fixture.service.host_slots_for_test() == 0 && !running.is_finished());
        ensure!(
            bound
                .fixture
                .service
                .admission
                .ingress_for_test()
                .map_err(|code| anyhow::anyhow!("{code:?}"))?
                .2
                == 2
        );
        // Synchronous handoff installs the next real actor COMMIT barrier before
        // the released receipt transaction can schedule its Core continuation.
        bound.fixture.service.cleanup.release_gate();
        let outcome_gate = bound.fixture.service.cleanup.hold(Phase::Committing);
        tokio::time::timeout(TIMEOUT, outcome_gate.entered.notified()).await?;
        receipt_committed(&bound)?;
        ensure!(
            core.snapshot().await.context_store.decisions[id]
                .outcome
                .is_none()
        );
        ensure!(generation.calls() == 0 && bound.fixture.service.host_slots_for_test() == 0);
        running.abort();
        ensure!(running.await.is_err());
        let mut shutdown = Box::pin(bound.fixture.service.shutdown());
        ensure!(
            futures::poll!(&mut shutdown).is_pending(),
            "Core outcome ACK was replaced by child receipt completion"
        );
        ensure!(bound.fixture.service.host_slots_for_test() == 0);
        bound.fixture.service.cleanup.release_gate();
        tokio::time::timeout(TIMEOUT, &mut shutdown).await??;
        ensure!(
            core.snapshot().await.context_store.decisions[id]
                .outcome
                .as_ref()
                .is_some_and(Result::is_ok)
        );
        ensure!(generation.calls() == 0 && choice.calls() == 1);
        ensure!(
            bound.fixture.service.host_slots_for_test() == 1
                && bound.fixture.service.cleanup.disposition() != Disposition::Ambiguous
        );
        ensure!(fixture::journal(&bound).await?["released"] == true);
        Ok(())
    })
    .await
    .context("Choice independent ACK deadline")
    .and_then(|result| result);
    bound.fixture.service.cleanup.release_gate();
    let choice_closed = choice.finish().await;
    finish(&bound.fixture, generation, checked).await?;
    choice_closed
}

pub(in crate::tenancy::tests::execution::native::session) async fn exercise(
    root: &std::path::Path,
    mode: &str,
) -> Result<()> {
    if mode.starts_with("choice-ack-") {
        return super::readback::exercise(root, mode).await;
    }
    ensure!(matches!(
        mode,
        "choice-commit-error" | "choice-transaction-unwind"
    ));
    let generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::start(true, false).await?;
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace)?;
    let bound = fixture::sqlite(&generation, &choice, root, 2).await?;
    let mut session = fixture::open(
        &bound,
        &generation,
        &workspace,
        Arc::new(HostSpies::default()),
    )
    .await?;
    let core = session.core().clone();
    let running = AbortOnDropHandle::new(tokio::spawn(async move {
        session
            .run(
                fixture::task(),
                Arc::new(Approval),
                CancellationToken::new(),
            )
            .await
    }));
    choice.receive().await?;
    let saved = fixture::journal(&bound).await?;
    let (id, link) = saved["owned_choices"]
        .as_object()
        .context("intent")?
        .iter()
        .next()
        .context("intent link")?;
    let child = link["child_execution"].as_str().context("child ID")?;
    observe_child(&bound, child)?;
    if mode == "choice-commit-error" {
        // Real deferred constraint failure after paid Decisions and outcome DML.
        // https://www.sqlite.org/foreignkeys.html#fk_deferred
        // https://www.sqlite.org/json1.html#jtype
        for sql in [
            "PRAGMA foreign_keys=ON",
            "CREATE TABLE choice_parent (id INTEGER PRIMARY KEY)",
            "CREATE TABLE choice_child (id INTEGER REFERENCES choice_parent(id) DEFERRABLE INITIALLY DEFERRED)",
        ] {
            bound.fixture.db.execute_unprepared(sql).await?;
        }
        let path = format!("$.journal.owned_choices.\"{id}\".outcome_digest").replace('\'', "''");
        bound.fixture.db.execute_unprepared(&format!("CREATE TRIGGER choice_commit_fault AFTER UPDATE OF revision ON bro_managed_sessions WHEN json_type(NEW.payload, '{path}') = 'text' BEGIN INSERT INTO choice_child(id) VALUES (17); END")).await?;
    }
    let receipt_gate = bound.fixture.service.cleanup.hold(Phase::Committing);
    choice.release();
    tokio::time::timeout(TIMEOUT, receipt_gate.entered.notified()).await?;
    ensure!(
        bound
            .fixture
            .service
            .cleanup
            .receipt_trace_for_test()?
            .facts[0]
            .as_ref()
            .is_some_and(|fact| fact.store.is_some_and(|value| value.ok) && fact.commit.is_none())
    );
    bound.fixture.service.cleanup.release_gate();
    let outcome_gate = bound.fixture.service.cleanup.hold(Phase::Committing);
    tokio::time::timeout(TIMEOUT, outcome_gate.entered.notified()).await?;
    receipt_committed(&bound)?;
    ensure!(
        core.snapshot().await.context_store.decisions[id]
            .outcome
            .is_none()
            && generation.calls() == 0
    );
    if mode == "choice-transaction-unwind" {
        bound.fixture.service.cleanup.unwind_gate()?;
    } else {
        bound.fixture.service.cleanup.release_gate();
    }
    tokio::time::timeout(TIMEOUT, bound.fixture.service.cleanup.quarantined()).await?;
    running.abort();
    // The failed commit may already have reached the waiter before cancellation.
    // Neither successful execution nor a panicked waiter proves retained custody.
    match running.await {
        Err(error) => ensure!(error.is_cancelled(), "native waiter panicked"),
        Ok(outcome) => ensure!(outcome.is_err(), "native waiter unexpectedly succeeded"),
    }
    ensure!(
        bound.fixture.service.host_slots_for_test() == 0
            && bound.fixture.service.current().is_err()
    );
    let fence = core
        .fence_for_restoration(&core.head().await)
        .await
        .err()
        .context("retained Core owner was discarded")?;
    ensure!(fence.code == bitrouter_ai::core::protocol::ErrorCode::Busy);
    super::super::custody::no_clean_drain(&bound.fixture).await?;
    ensure!(
        bound
            .fixture
            .service
            .completion
            .clone()
            .now_or_never()
            .is_none()
    );
    ensure!(
        bound.fixture.service.host_slots_for_test() == 0
            && generation.calls() == 0
            && choice.calls() == 1
    );
    let persisted = tokio::time::timeout(TIMEOUT, fixture::journal(&bound)).await??;
    ensure!(
        persisted["owned_choices"][id]["outcome_digest"].is_null()
            && persisted["released"] == false
    );
    bound.fixture.db.clone().close().await?;
    choice.finish().await?;
    generation.finish().await?;
    // All ordinary handles drop before the existing isolated parent probes the
    // authority file. Only actual H/native/Choice custody may retain that lease.
    Ok(())
}

#[tokio::test]
async fn owned_choice_unknown_outcome_commit_retains_actual_core_owner_and_file_lease() -> Result<()>
{
    for mode in ["choice-commit-error", "choice-transaction-unwind"] {
        super::super::custody::isolated(mode).await?;
    }
    Ok(())
}
