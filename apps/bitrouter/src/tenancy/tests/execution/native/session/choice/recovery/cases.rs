use super::*;
use crate::storage::checkpoint::CheckpointBatchExt;

pub(super) async fn campaign(root: &Path, mode: &str) -> Result<()> {
    ensure!(matches!(
        mode,
        "completed-child"
            | "missing-child"
            | "precommit-rollback"
            | "commit-error"
            | "revoked-after-prepare"
            | "repair-commit-crash"
            | "legacy-unreleased"
            | "missing-seed"
    ));
    let mut generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::start(true, false).await?;
    std::fs::create_dir(root.join("workspace"))?;
    let initial = fixture::sqlite(&generation, &choice, root, 2).await?;
    let spec = Spec::capture(&initial, &generation)?;
    private_json(&root.join("spec.json"), &spec)?;
    initial.fixture.service.shutdown().await?;
    initial.fixture.db.clone().close().await?;
    drop(initial);
    // Pool hooks may still finish their close continuation; do not substitute a
    // fresh sidecar or a synthetic stop flag when the real owner remains held.
    tokio::time::timeout(TIMEOUT, async {
        loop {
            match crate::tenancy::ownership::acquire_path(&root.join("owned.db"), false) {
                Ok(owner) => {
                    drop(owner);
                    break Ok::<_, anyhow::Error>(());
                }
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
    let mut child = Child::start("native_stopped_owner_producer", root, mode)?;
    let observed = async {
        child.line("native-recovery-intent").await?;
        choice.receive().await?;
        ensure!(choice.calls() == 1 && generation.calls() == 0);
        let error = crate::tenancy::ownership::acquire_path(&root.join("owned.db"), false)
            .err()
            .context("live child lost file ownership")?;
        ensure!(matches!(
            error.downcast_ref::<std::fs::TryLockError>(),
            Some(std::fs::TryLockError::WouldBlock)
        ));
        if mode == "completed-child" {
            choice.release();
            child.line("native-recovery-terminal").await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let reaped = child.reap(true).await;
    if let Err(error) = observed.and(reaped) {
        let closed = choice.finish().await;
        let generation_closed = generation.finish().await;
        closed?;
        generation_closed?;
        return Err(error);
    }
    if mode == "repair-commit-crash" {
        let mut repairer = Child::start("native_stopped_owner_repairer", root, mode)?;
        let observed = repairer
            .line("native-repair-committed-before-retirement")
            .await;
        let reaped = repairer.reap(true).await;
        observed?;
        reaped?;
    }
    let bound = spec.connect(root).await?;
    if matches!(
        mode,
        "precommit-rollback" | "commit-error" | "revoked-after-prepare"
    ) {
        let result = faults::exercise(&bound, &spec, root, mode, &generation, &choice).await;
        let choice_closed = choice.finish().await;
        if mode == "commit-error" {
            // This deliberately retained owner exits with the isolated runtime.
            // No successful Service shutdown or clean transaction is asserted.
            let generation_closed = generation.finish().await;
            result?;
            choice_closed?;
            generation_closed?;
        } else {
            finish(&bound.fixture, generation, result).await?;
            bound.fixture.service.cleanup.ensure_clean()?;
            choice_closed?;
        }
        return Ok(());
    }
    if matches!(mode, "legacy-unreleased" | "missing-seed") {
        let result = faults::unsupported(&bound, root, mode, &generation, &choice).await;
        let choice_closed = choice.finish().await;
        finish(&bound.fixture, generation, result).await?;
        bound.fixture.service.cleanup.ensure_clean()?;
        choice_closed?;
        return Ok(());
    }
    let checked = async {
        let before:Value=serde_json::from_slice(&std::fs::read(root.join("intent.json"))?)?;
        let (decision,link)=before["owned_choices"].as_object().context("persisted intent")?.iter().next().context("persisted link")?;
        let parent=link["parent_execution"].as_str().context("parent")?;
        let child=link["child_execution"].as_str().context("child")?;
        let source_head: bitrouter_ai::core::checkpoint::DurableHead = serde_json::from_value(before["head"].clone())?;
        let source_batch: bitrouter_ai::core::checkpoint::CheckpointBatch = serde_json::from_value(before["checkpoint"].clone())?;
        let limits: bitrouter_ai::core::protocol::Limits = serde_json::from_value(before["limits"].clone())?;
        let source_state: SessionSnapshot = serde_json::from_value(source_batch.decode(&limits)?.checkpoint.state)?;
        let source_run = source_state.run.as_ref().context("stopped root run")?;
        let step_id = link["step_id"].as_str().context("actual Choice step")?;
        let step = source_state.agents.values().filter_map(|agent| agent.turn.as_ref())
            .flat_map(|turn| &turn.steps).find(|step| step.step_id == step_id)
            .context("actual preparation step")?;
        use crate::gateway::pipeline::native_preparation::NativePreparationWorkKind;
        let kinds = [NativePreparationWorkKind::PreResolutionHook,
            NativePreparationWorkKind::PreResolutionHook, NativePreparationWorkKind::RouterLookup];
        ensure!(step.preparation_work.len() == kinds.len()
            && step.preparation_work.iter().zip(kinds).enumerate().all(|(index, (record, kind))|
                record.work.kind == kind && record.work.work_index == index as u32
                && record.work.request_id == parent && record.report.as_ref().is_some_and(|report|
                    report.work == record.work && report.error_code.is_none())),
            "real stopped Choice lacked its acknowledged parent preparation prefix");
        let mut restored=fixture::open(&bound,&generation,&root.join("workspace"),Arc::new(HostSpies::default())).await?;
        let state=restored.core().snapshot().await;
        let retired = state.run.as_ref().context("retired root run")?;
        let gap = state.activity_gaps.get(&retired.run_id).context("retained activity gap")?;
        ensure!(retired.run_id == source_run.run_id
            && gap.durable_head == source_head
            && gap.known_active_ms == source_run.active_ms
            && retired.active_ms >= source_run.active_ms
            && retired.activity_reconciliations == source_run.activity_reconciliations,
            "retirement fabricated activity evidence or lost the original gap");
        let interrupted=state.context_store.decisions[decision].outcome.as_ref().context("reconciled Choice")?
            .as_ref().err().context("completed child was fabricated into a Decisions answer")?;
        ensure!(interrupted.kind==crate::gateway::model::decision::types::DecisionFailure::Interrupted
            && interrupted.usage.is_none() && interrupted.may_have_run);
        ensure!(state.run.as_ref().is_some_and(|run|run.status==RunStatus::Cancelled));
        ensure!(state.root_queue.pending.is_empty() && generation.calls()==0 && choice.calls()==1);
        let saved=fixture::journal(&bound).await?;
        ensure!(saved["owned_choices"][decision]["terminal_digests"].as_array().is_some_and(|v|v.len()==2));
        if mode=="repair-commit-crash" {
            let before:Vec<(String,String,String)>=serde_json::from_slice(&std::fs::read(root.join("repair-terminals.json"))?)?;
            let rows=bound.fixture.db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
                "SELECT execution_id,receipt_json,disposition_json FROM tenancy_execution_receipts ORDER BY execution_id")).await?;
            let after=rows.iter().map(|row|Ok((row.try_get::<String>("","execution_id")?,
                row.try_get::<String>("","receipt_json")?,row.try_get::<String>("","disposition_json")?))).collect::<Result<Vec<_>>>()?;
            ensure!(before==after,"second recovery rewrote committed terminals");
        }
        let parent_receipt=receipt(&bound,parent).await?;
        ensure!(parent_receipt.state==bitrouter_ai::tenancy::execution::ReceiptState::Unknown
            && parent_receipt.attempts.is_empty() && parent_receipt.correlation_id.as_str()=="same-correlation"
            && parent_receipt.execution_revision==bound.revision);
        let child_receipt=receipt(&bound,child).await?;
        if mode=="completed-child" {
            let expected:(String,String)=serde_json::from_slice(&std::fs::read(root.join("receipt-before.json"))?)?;
            let actual=bound.fixture.db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT receipt_json,disposition_json FROM tenancy_execution_receipts WHERE installation_id=? AND tenant_id=? AND execution_id=?",
                [bound.fixture.installation.as_str().into(),bound.fixture.tenant.as_str().into(),child.into()])).await?.context("preserved terminal")?;
            ensure!((actual.try_get::<String>("","receipt_json")?,actual.try_get::<String>("","disposition_json")?)==expected);
            ensure!(child_receipt.attempts.len()==1 && child_receipt.outcome==Outcome::Completed);
        } else {
            ensure!(child_receipt.state==bitrouter_ai::tenancy::execution::ReceiptState::Unknown
                && child_receipt.attempts.is_empty() && child_receipt.usage.provenance==Provenance::Unknown
                && !child_receipt.usage.complete && child_receipt.usage.input_tokens.is_none()
                && child_receipt.usage.output_tokens.is_none());
            choice.release();
        }
        let new=restored.run(fixture::task(),Arc::new(Approval),CancellationToken::new()).await?;
        ensure!(new.run.as_ref().is_some_and(|run|run.status==RunStatus::Completed));
        ensure!(new.run.as_ref().is_some_and(|run| !new.activity_gaps.contains_key(&run.run_id)
            && run.activity_reconciliations.is_empty() && run.run_id != retired.run_id));
        ensure!(new.activity_gaps == state.activity_gaps,
            "a new root erased the predecessor's activity uncertainty");
        generation.receive().await?;choice.receive().await?;
        ensure!(generation.calls()==1 && choice.calls()==2);
        let new_links=fixture::journal(&bound).await?;
        ensure!(new_links["owned_choices"].as_object().context("new Choice")?.values()
            .any(|record|record["child_execution"]!=child && record["parent_execution"]!=parent));
        restored.close().await?;
        let reopened = fixture::open(&bound,&generation,&root.join("workspace"),Arc::new(HostSpies::default())).await?;
        ensure!(reopened.core().snapshot().await.activity_gaps == state.activity_gaps,
            "native journal reopen lost retired activity uncertainty");
        reopened.close().await?;
        tokio::time::timeout(TIMEOUT,bound.fixture.service.admission.wait_drained()).await?;
        ensure!(bound.fixture.service.host_slots_for_test()==1);
        Ok(())
    }.await;
    let choice_closed = choice.finish().await;
    finish(&bound.fixture, generation, checked).await?;
    bound.fixture.service.cleanup.ensure_clean()?;
    choice_closed
}
