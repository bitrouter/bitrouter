//! Actual Core callbacks keep their post-COMMIT ACK after pipeline cancellation.
use super::*;
use crate::tenancy::{
    hosts::{HostBoundary, HostGate},
    receipts::read,
};
use bitrouter_ai::{
    core::checkpoint::DurableHead,
    tenancy::{
        execution::{ExecutionReceipt, Outcome, ReceiptState},
        identity::ExecutionId,
    },
};

struct Release(Arc<HostGate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

async fn journal(fixture: &Fixture) -> Result<Value> {
    let row = fixture.db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT installation_id, tenant_id, session_principal_id, payload, payload_bytes FROM bro_managed_sessions"
    )).await?.context("actual owned journal")?;
    ensure!(row.try_get::<String>("", "installation_id")? == fixture.installation.as_str());
    ensure!(row.try_get::<String>("", "tenant_id")? == fixture.tenant.as_str());
    ensure!(row.try_get::<String>("", "session_principal_id")? == fixture.principal.as_str());
    let bytes = row.try_get::<String>("", "payload")?;
    ensure!(row.try_get::<i64>("", "payload_bytes")? == i64::try_from(bytes.len())?);
    Ok(serde_json::from_str::<Value>(&bytes)?["journal"].clone())
}

async fn held_ack(completed_provider: bool) -> Result<()> {
    let mut provider = Provider::start(false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, directory.path()).await?;
    let checked = async {
        let (headers, _) = ready(&fixture, &route).await?;
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let session = NativeSession::open_authenticated(
            app,
            headers,
            "callback-ack",
            &workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await?;
        // The public facade supplies the real checked Core and native port. Use
        // its normal Core start/drive consumer, also used by embedding callers.
        // No replacement harness, grant, receipt or checkpoint proof is created.
        let core = session.core().clone();
        core.start("callback-root", core.head().await.state_revision, input())
            .await?;
        let boundary = if completed_provider {
            HostBoundary::NativeAttemptOutcomeAck
        } else {
            HostBoundary::NativeAttemptIntentAck
        };
        let gate = fixture.service.hold_host(boundary)?;
        let release = Release(gate.clone());
        let driver_core = core.clone();
        let mut driving =
            AbortOnDropHandle::new(tokio::spawn(async move { driver_core.drive().await }));
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        let before = core.snapshot().await;
        let step = before
            .agents
            .get(&before.agent_id)
            .and_then(|agent| agent.turn.as_ref())
            .and_then(|turn| turn.steps.last())
            .context("actual Core model step")?;
        let id: ExecutionId = step
            .plan
            .as_ref()
            .context("actual Core plan")?
            .request_id
            .clone()
            .try_into()?;
        ensure!(step.attempts.len() == usize::from(completed_provider));
        ensure!(
            step.attempts
                .iter()
                .all(|attempt| attempt.receipt.is_none())
        );
        let saved = journal(&fixture).await?;
        let committed: DurableHead = serde_json::from_value(saved["head"].clone())?;
        ensure!(
            committed.state_revision > core.head().await.state_revision,
            "barrier did not follow a real committed checkpoint"
        );
        let (receipt, tasks) = fixture.service.pipelines.callback_owner_for_test(&id)?;
        ensure!(provider.calls() == usize::from(completed_provider));
        if completed_provider {
            ensure!(provider.receive().await?.body["model"] == "model-a");
        }
        receipt.cancel();
        let cleanup_deadline = receipt.cleanup_deadline();
        // Wait for the actual cancelled preparation task and its sole Source to
        // finish. A sleep or receipt-cancel notification alone cannot establish
        // that the enclosing future has been dropped.
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let pending = tasks.pending();
                if pending.executions == 0 && pending.sources == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        ensure!(
            tasks.pending().settlements > 0,
            "cancelled caller lost its pending callback ACK"
        );
        ensure!(
            fixture
                .service
                .admission
                .wait_drained()
                .now_or_never()
                .is_none()
        );
        ensure!(fixture.service.host_slots_for_test() == 0);
        let busy =
            crate::tenancy::ownership::acquire_path(&directory.path().join("owned.db"), true)
                .err()
                .context("held callback lost the original file owner")?;
        ensure!(matches!(
            busy.downcast_ref::<std::fs::TryLockError>(),
            Some(std::fs::TryLockError::WouldBlock)
        ));
        ensure!(receipt.cleanup_deadline() == cleanup_deadline);
        ensure!(core.head().await.state_revision < committed.state_revision);
        ensure!(
            journal(&fixture).await? == saved,
            "cancel replayed or modified the held journal"
        );
        ensure!(
            !driving.is_finished(),
            "driver escaped the retained checkpoint boundary"
        );
        gate.release();
        let driven = tokio::time::timeout(TIMEOUT, &mut driving).await???;
        // Delivery is cancelled, but the original callback must actually adopt
        // the committed head before later failure/cleanup transitions proceed.
        let after = core.snapshot().await;
        let after_step = after
            .agents
            .get(&after.agent_id)
            .and_then(|agent| agent.turn.as_ref())
            .and_then(|turn| {
                turn.steps
                    .iter()
                    .find(|candidate| candidate.step_id == step.step_id)
            })
            .context("retained model step")?;
        ensure!(core.head().await.state_revision >= committed.state_revision);
        ensure!(
            after_step.attempts.len() == 1,
            "late callback repeated an intent"
        );
        let attempt = &after_step.attempts[0];
        if completed_provider {
            let evidence = attempt
                .receipt
                .as_ref()
                .context("real late outcome ACK not adopted")?;
            ensure!(evidence.report.request_id == id.as_str() && evidence.report.result.is_some());
        } else {
            ensure!(
                attempt.receipt.is_none(),
                "unsent intent invented provider evidence"
            );
        }
        ensure!(
            driven.run.as_ref().is_some_and(
                |run| run.status == crate::storage::checkpoint::snapshot::RunStatus::Failed
            ),
            "cancelled delivery did not settle as a failed run"
        );
        tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
        let bytes = fixture
            .service
            .read_receipt(
                fixture.operator.clone(),
                fixture.tenant.clone(),
                read::Request::One(id.clone()),
            )
            .await
            .map_err(failure)?;
        let durable: ExecutionReceipt = serde_json::from_slice(&bytes)?;
        ensure!(
            durable.execution_id == id
                && durable.tenant_id == fixture.tenant
                && durable.installation_id == fixture.installation
        );
        ensure!(durable.attempts.len() == usize::from(completed_provider));
        if completed_provider {
            ensure!(durable.attempts[0].outcome == Outcome::Completed);
            ensure!(
                durable.attempts[0].usage.input_tokens == Some(4)
                    && durable.attempts[0].usage.output_tokens == Some(2)
            );
            // The generator omits cache/reasoning dimensions. Successful work
            // does not justify inventing known zero usage or a Committed state.
            ensure!(durable.state == ReceiptState::Incomplete);
        }
        ensure!(
            provider.calls() == usize::from(completed_provider),
            "late ACK admitted a send"
        );
        drop(release);
        tokio::time::timeout(TIMEOUT, session.close()).await??;
        fixture.service.cleanup.ensure_clean()?;
        ensure!(fixture.service.host_slots_for_test() == 1);
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await?;
    fixture.db.clone().close().await?;
    drop(fixture);
    // Join shutdown and drop all ordinary handles before proving the same file
    // can be owned again. Merely observing a cancelled waiter is insufficient.
    let owner = tokio::time::timeout(TIMEOUT, async {
        loop {
            match crate::tenancy::ownership::acquire_path(&directory.path().join("owned.db"), true)
            {
                Ok(owner) => break Ok(owner),
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    tokio::task::yield_now().await
                }
                Err(error) => break Err(error),
            }
        }
    })
    .await??;
    drop(owner);
    Ok(())
}

#[tokio::test]
async fn cancelled_preparation_retains_actual_intent_ack_without_late_provider_send() -> Result<()>
{
    held_ack(false).await
}

#[tokio::test]
async fn cancelled_pipeline_retains_actual_model_outcome_ack_and_original_usage() -> Result<()> {
    held_ack(true).await
}
