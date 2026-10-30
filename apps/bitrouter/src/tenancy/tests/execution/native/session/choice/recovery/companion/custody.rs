//! Actual repair transaction ownership survives caught unwind and a late COMMIT.
use super::*;
use crate::tenancy::transaction::Disposition;

#[tokio::test]
#[ignore = "isolated actual native repair H custody; select this parent"]
async fn native_recovery_unwind_and_overdue_commit_keep_the_exact_owner() -> Result<()> {
    campaign(&["custody-unwind", "custody-late-commit"]).await
}

pub(super) async fn case(root: &Path, mode: &str) -> Result<()> {
    let stopped = Stopped::produce(root, mode).await?;
    let result = async {
        let before = payload(&stopped.bound.fixture.db).await?;
        let journal = fixture::journal(&stopped.bound).await?;
        let link = journal["owned_choices"]
            .as_object()
            .context("real intent")?
            .values()
            .next()
            .context("real Choice link")?;
        let child = link["child_execution"]
            .as_str()
            .context("child execution")?;
        stopped
            .bound
            .fixture
            .service
            .cleanup
            .observe_receipt_for_test(Key {
                installation: stopped.bound.fixture.installation.clone(),
                tenant: stopped.bound.fixture.tenant.clone(),
                execution: child.try_into()?,
            })?;
        let resources = stopped
            .bound
            .fixture
            .service
            .hold_host(crate::tenancy::hosts::HostBoundary::Resources)?;
        let app = Arc::new(
            host_app(
                &stopped.bound.fixture,
                &stopped.generation.url,
                Arc::new(HostSpies::default()),
            )?
            .with_tenancy(stopped.bound.fixture.service.clone()),
        );
        let headers = stopped.bound.headers.clone();
        let workspace = root.join("workspace");
        let opening = AbortOnDropHandle::new(tokio::spawn(async move {
            NativeSession::open_authenticated(
                app,
                headers,
                "choice-session",
                &workspace,
                ToolMode::ReadOnly,
                &HarnessConfig::default(),
            )
            .await
        }));
        let checked = async {
            tokio::time::timeout(TIMEOUT, resources.entered.notified()).await?;
            let commit = stopped
                .bound
                .fixture
                .service
                .cleanup
                .hold(Phase::Committing);
            resources.release();
            tokio::time::timeout(TIMEOUT, commit.entered.notified()).await?;
            ensure!(stopped.bound.fixture.service.cleanup.disposition() == Disposition::Pending);
            ensure!(stopped.bound.fixture.service.host_slots_for_test() == 0);
            ensure!(
                stopped
                    .bound
                    .fixture
                    .service
                    .admission
                    .receipt_capacity_for_test()
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?
                    .0
                    > 0
            );
            locked(root)?;
            if mode == "custody-unwind" {
                stopped.bound.fixture.service.cleanup.unwind_gate()?;
            } else {
                // Let the real native writer deadline expire while its actual
                // consuming COMMIT future remains held. No phase is fabricated.
                tokio::time::timeout(TIMEOUT, stopped.bound.fixture.service.cleanup.wait_fenced())
                    .await?;
                ensure!(
                    stopped.bound.fixture.service.cleanup.disposition() == Disposition::Pending
                );
                ensure!(stopped.bound.fixture.service.current().is_err());
                locked(root)?;
                stopped.bound.fixture.service.cleanup.release_gate();
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        resources.release();
        stopped.bound.fixture.service.cleanup.release_gate();
        checked?;
        if mode == "custody-unwind" {
            tokio::time::timeout(TIMEOUT, stopped.bound.fixture.service.cleanup.quarantined())
                .await?;
        } else {
            tokio::time::timeout(TIMEOUT, async {
                loop {
                    let trace = stopped
                        .bound
                        .fixture
                        .service
                        .cleanup
                        .receipt_trace_for_test()?;
                    ensure!(!trace.overflow);
                    if trace.facts[0]
                        .as_ref()
                        .is_some_and(|fact| fact.commit.is_some())
                    {
                        return Ok::<_, anyhow::Error>(());
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await??;
        }
        let opened = tokio::time::timeout(TIMEOUT, opening).await??;
        if let Ok(session) = opened {
            session.close().await?;
            anyhow::bail!("faulted recovery returned usable authority");
        }
        let reader = crate::db::connect(&format!(
            "sqlite://{}?mode=ro",
            root.join("owned.db").display()
        ))
        .await?;
        let readback = async {
            let trace = stopped
                .bound
                .fixture
                .service
                .cleanup
                .receipt_trace_for_test()?;
            ensure!(!trace.overflow);
            let fact = trace.facts[0]
                .as_ref()
                .context("actual receipt insert was not reached")?;
            ensure!(
                fact.inserted == Some(true)
                    && fact.store.is_some_and(|r| r.ok)
                    && fact.native_checkpoint.is_some_and(|r| r.ok)
            );
            if mode == "custody-unwind" {
                ensure!(fact.commit.is_none() && fact.rollback.is_none());
                ensure!(
                    stopped.bound.fixture.service.cleanup.disposition() == Disposition::Ambiguous
                );
                ensure!(
                    stopped
                        .bound
                        .fixture
                        .service
                        .cleanup
                        .ensure_clean()
                        .is_err()
                );
                ensure!(payload(&reader).await? == before && terminals(&reader).await?.is_empty());
                ensure!(stopped.bound.fixture.service.host_slots_for_test() == 0);
                ensure!(
                    stopped
                        .bound
                        .fixture
                        .service
                        .admission
                        .receipt_capacity_for_test()
                        .map_err(|e| anyhow::anyhow!("{e:?}"))?
                        .0
                        > 0
                );
                ensure!(
                    stopped
                        .bound
                        .fixture
                        .service
                        .completion
                        .clone()
                        .now_or_never()
                        .is_none()
                );
            } else {
                ensure!(fact.commit.is_some_and(|r| r.ok) && fact.rollback.is_none());
                let receipts = terminals(&reader).await?;
                ensure!(receipts.len() == 2);
                for (_, bytes, _) in &receipts {
                    let receipt: ExecutionReceipt = serde_json::from_str(bytes)?;
                    ensure!(
                        receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Unknown
                            && receipt.attempts.is_empty()
                    );
                }
                ensure!(
                    stopped
                        .bound
                        .fixture
                        .service
                        .admission
                        .receipt_capacity_for_test()
                        .map_err(|e| anyhow::anyhow!("{e:?}"))?
                        .0
                        == 0,
                    "late acknowledged commit did not publish accounting"
                );
                // The repair committed, but its caller missed the ACK and H
                // fenced authority. Failed-open readback cannot establish host
                // cleanup on that fenced Service; the same owner must stay held.
                ensure!(fact.disposition == Disposition::Committed);
                tokio::time::timeout(TIMEOUT, stopped.bound.fixture.service.cleanup.quarantined())
                    .await?;
                ensure!(
                    stopped
                        .bound
                        .fixture
                        .service
                        .cleanup
                        .ensure_clean()
                        .is_err()
                );
                ensure!(
                    stopped
                        .bound
                        .fixture
                        .service
                        .host_cleanup_unknown_site_for_test()
                        .is_some()
                        && stopped.bound.fixture.service.host_slots_for_test() == 0
                        && stopped
                            .bound
                            .fixture
                            .service
                            .completion
                            .clone()
                            .now_or_never()
                            .is_none()
                );
                let row: Value = serde_json::from_str(&payload(&reader).await?)?;
                ensure!(
                    row["recovery"]["phase"] == "retiring" && row["journal"]["released"] == false
                );
                ensure!(
                    stopped.bound.fixture.service.current().is_err(),
                    "late commit reopened dispatch"
                );
            }
            locked(root)?;
            ensure!(stopped.choice.calls() == 1 && stopped.generation.calls() == 0);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let closed = reader.close().await;
        readback?;
        closed?;
        Ok(())
    }
    .await;
    // An uncertain H owner deliberately lives until this isolated controller
    // exits. Do not call clean Service shutdown or treat Drop as a rollback ACK.
    let Stopped {
        generation,
        mut choice,
        bound: _,
        spec: _,
    } = stopped;
    choice.release();
    let choice_closed = choice.finish().await;
    let generation_closed = generation.finish().await;
    result?;
    choice_closed?;
    generation_closed
}
