use super::*;
use crate::tenancy::transaction::Disposition;

#[tokio::test]
async fn owned_choice_lost_outcome_ack_requires_exact_complete_journal_readback() -> Result<()> {
    for mode in ["choice-ack-exact", "choice-ack-link", "choice-ack-artifact"] {
        super::super::custody::isolated(mode).await?;
    }
    Ok(())
}

pub(super) async fn exercise(root: &std::path::Path, mode: &str) -> Result<()> {
    ensure!(matches!(
        mode,
        "choice-ack-exact" | "choice-ack-link" | "choice-ack-artifact"
    ));
    let generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::start(false, false).await?;
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
    let gate = bound
        .fixture
        .service
        .hold_host(crate::tenancy::hosts::HostBoundary::ChoiceOutcomeAck)?;
    let running = AbortOnDropHandle::new(tokio::spawn(async move {
        session
            .run(
                fixture::task(),
                Arc::new(Approval),
                CancellationToken::new(),
            )
            .await
    }));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    choice.receive().await?;
    let before = fixture::journal(&bound).await?;
    let links = before["owned_choices"]
        .as_object()
        .context("same journal links")?;
    let (id, link) = links.iter().next().context("durable outcome")?;
    ensure!(links.len() == 1 && link["outcome_digest"].is_string());
    ensure!(
        core.snapshot()
            .await
            .context_store
            .decisions
            .get(id)
            .is_some_and(|receipt| receipt.outcome.is_none())
    );
    let child = link["child_execution"].as_str().context("child ID")?;
    ensure!(receipt(&bound, child).await?.attempts.len() == 1);
    ensure!(
        choice.calls() == 1
            && generation.calls() == 0
            && bound.fixture.service.host_slots_for_test() == 0
    );
    if mode != "choice-ack-exact" {
        let row = bound
            .fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT payload FROM bro_managed_sessions",
            ))
            .await?
            .context("committed journal")?;
        let mut envelope: Value = serde_json::from_str(&row.try_get::<String>("", "payload")?)?;
        if mode == "choice-ack-link" {
            envelope["journal"]["owned_choices"][id]["native_digest"] = json!("0".repeat(64));
        } else {
            use base64::Engine;
            let bytes = b"unrelated Choice readback evidence";
            envelope["journal"]["artifacts"]["unrelated-choice-evidence"] = json!({
                "reference":{"artifact_id":"unrelated-choice-evidence","sha256":crate::storage::checkpoint::sha256(bytes),
                    "bytes":bytes.len(),"media_type":"text/plain"},
                "content_base64":base64::engine::general_purpose::STANDARD.encode(bytes)
            });
        }
        let bytes = serde_json::to_string(&envelope)?;
        // Corrupt the actual committed row while preserving every SQL length,
        // descriptor, checkpoint and head field; there is no fake store reply.
        bound
            .fixture
            .db
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE bro_managed_sessions SET payload=?, payload_bytes=?",
                [bytes.clone().into(), i64::try_from(bytes.len())?.into()],
            ))
            .await?;
        ensure!(fixture::journal(&bound).await?["checkpoint"] == before["checkpoint"]);
        ensure!(fixture::journal(&bound).await?["head"] == before["head"]);
    }
    gate.release();
    let outcome = tokio::time::timeout(TIMEOUT, running).await??;
    ensure!(outcome.is_err(), "lost ACK resumed provider dispatch");
    if mode == "choice-ack-exact" {
        tokio::time::timeout(TIMEOUT, async {
            while bound.fixture.service.host_slots_for_test() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        ensure!(
            core.snapshot().await.context_store.decisions[id]
                .outcome
                .as_ref()
                .is_some_and(|value| value.is_ok())
        );
        ensure!(fixture::journal(&bound).await?["owned_choices"] == before["owned_choices"]);
        ensure!(fixture::journal(&bound).await?["released"] == true);
        ensure!(bound.fixture.service.current().is_ok());
        bound.fixture.service.shutdown().await?;
        bound.fixture.service.cleanup.ensure_clean()?;
    } else {
        tokio::time::timeout(TIMEOUT, bound.fixture.service.cleanup.quarantined()).await?;
        ensure!(
            bound.fixture.service.current().is_err()
                && bound.fixture.service.host_slots_for_test() == 0
        );
        ensure!(bound.fixture.service.cleanup.disposition() == Disposition::Ambiguous);
        ensure!(
            core.fence_for_restoration(&core.head().await)
                .await
                .is_err()
        );
        ensure!(
            core.snapshot().await.context_store.decisions[id]
                .outcome
                .is_none()
        );
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
        ensure!(fixture::journal(&bound).await?["released"] == false);
    }
    ensure!(
        generation.calls() == 0 && choice.calls() == 1,
        "readback retried paid work"
    );
    bound.fixture.db.clone().close().await?;
    choice.finish().await?;
    generation.finish().await?;
    // The existing isolated parent probes the actual OS lease after all ordinary
    // handles leave this frame. Unknown cleanup keeps the retained driver/host.
    Ok(())
}
