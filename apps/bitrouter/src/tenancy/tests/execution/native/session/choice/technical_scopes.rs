//! Real Choice admissions and two durable protected reads. Post-ACK fixture
//! barriers grant a quiet window, not production retention or authority.
use super::*;
use bitrouter_ai::tenancy::{
    identity::ExecutionId, provider_technical_facts::ProviderTechnicalFactsV1,
    provider_technical_facts_v2::ProviderTechnicalFactsV2, validation::Validate,
};

async fn scoped(
    bound: &Bound,
    caller: &crate::tenancy::model::Caller,
    id: &ExecutionId,
) -> Result<(Vec<u8>, ProviderTechnicalFactsV2)> {
    let bytes = bound
        .fixture
        .service
        .read_receipt(
            caller.clone(),
            bound.fixture.tenant.clone(),
            read::Request::ProviderTechnicalV2(id.clone()),
        )
        .await
        .map_err(failure)?;
    let value: Option<ProviderTechnicalFactsV2> = serde_json::from_slice(&bytes)?;
    Ok((
        bytes,
        value.context("protected v2 getter has no durable object")?,
    ))
}
async fn operation_view(
    bound: &Bound,
    caller: &crate::tenancy::model::Caller,
    id: &ExecutionId,
) -> Result<Option<ProviderTechnicalFactsV1>> {
    let bytes = bound
        .fixture
        .service
        .read_receipt(
            caller.clone(),
            bound.fixture.tenant.clone(),
            read::Request::ProviderTechnical(id.clone()),
        )
        .await
        .map_err(failure)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[tokio::test]
async fn real_choice_parent_and_child_durable_getters_keep_own_context_mode_and_original_edge()
-> Result<()> {
    let mut generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::start(true, false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let bound = fixture::sqlite(&generation, &choice, directory.path(), 2).await?;
    let spies = Arc::new(HostSpies::default());
    let cancel = CancellationToken::new();
    // Retain actual caller/barrier custody outside the bounded workload.
    let mut release = Vec::with_capacity(2);
    let mut running = None;
    let mut returned = None;
    let checked = tokio::time::timeout(Duration::from_secs(25), async {
        let caller = bound
            .fixture
            .service
            .authenticate(
                bound
                    .key
                    .secret
                    .as_deref()
                    .context("original issued credential")?,
            )
            .await
            .map_err(failure)?;
        let mut session = fixture::open(&bound, &generation, &workspace, spies.clone()).await?;
        let core = session.core().clone();
        let cancellation = cancel.clone();
        running = Some(AbortOnDropHandle::new(tokio::spawn(async move {
            let result = session
                .run(fixture::task(), Arc::new(Approval), cancellation)
                .await;
            (session, result)
        })));
        let sent = choice.receive().await?;
        let intent = fixture::journal(&bound).await?;
        let links = intent["owned_choices"]
            .as_object()
            .context("durable Choice intent")?;
        ensure!(links.len() == 1);
        let (decision_id, link) = links.iter().next().context("original Choice link")?;
        let parent: ExecutionId = link["parent_execution"]
            .as_str()
            .context("actual parent admission")?
            .try_into()?;
        let child: ExecutionId = link["child_execution"]
            .as_str()
            .context("actual child admission")?
            .try_into()?;
        ensure!(parent != child && choice.calls() == 1 && generation.calls() == 0);
        // IDs come from the actual committed intent. The barrier checks which
        // actual terminal_receipt call returned; it sets no issuer/edge/proof.
        let child_ack = bound
            .fixture
            .service
            .technical_io
            .hold_after_receipt_ack(&child)?;
        release.push(child_ack.release.clone());
        choice.release();
        tokio::time::timeout(TIMEOUT, child_ack.entered.cancelled())
            .await
            .context("actual child receipt ACK barrier not reached")?;
        tokio::time::timeout(TIMEOUT, bound.fixture.service.technical_io.wait(&child)).await??;
        let child_receipt = receipt(&bound, child.as_str()).await?;
        let (child_bytes, child_scope) = scoped(&bound, &caller, &child).await?;
        child_scope.validate_against(&child_receipt)?;
        let child_context = child_scope
            .pipeline_scope
            .as_ref()
            .context("child owning context")?;
        // AdmittedChoice is typed Decisions, with no Generation native control.
        ensure!(
            child_context.context_request_id == child.as_str()
                && !child_context.streamed
                && child_context.stream.is_none()
        );
        ensure!(child_scope.execution_id == child && child_scope.operations.len() == 1);
        ensure!(
            child_scope.operations[0].attempt_ids
                == vec![child_receipt.attempts[0].attempt_id.clone()]
        );
        ensure!(
            child_scope.hop_observations.len() == 1
                && child_scope.hop_observations[0].attempt_ids
                    == child_scope.operations[0].attempt_ids
        );
        ensure!(operation_view(&bound, &caller, &child).await? == child_scope.operation_view());
        // Read actual DB-backed getter bytes, not captured(). The Core outcome
        // has not yet returned through this held caller; the child ACK is real.
        ensure!(
            core.snapshot().await.context_store.decisions[decision_id]
                .outcome
                .is_none()
        );
        ensure!(generation.calls() == 0 && !running.as_ref().context("native task")?.is_finished());
        let parent_ack = bound
            .fixture
            .service
            .technical_io
            .hold_after_receipt_ack(&parent)?;
        release.push(parent_ack.release.clone());
        child_ack.release.cancel();
        let generation_request = generation.receive().await?;
        ensure!(
            generation_request.path == "/v1/chat/completions"
                && generation_request.body["model"] == "model-a"
                && generation_request.body["max_tokens"] == 25
        );
        tokio::time::timeout(TIMEOUT, parent_ack.entered.cancelled())
            .await
            .context("actual parent receipt ACK barrier not reached")?;
        tokio::time::timeout(TIMEOUT, bound.fixture.service.technical_io.wait(&parent)).await??;
        let parent_receipt = receipt(&bound, parent.as_str()).await?;
        let (parent_bytes, parent_scope) = scoped(&bound, &caller, &parent).await?;
        parent_scope.validate_against(&parent_receipt)?;
        ensure!(parent_scope.execution_id == parent && parent_scope.operations.len() == 1);
        let parent_context = parent_scope
            .pipeline_scope
            .as_ref()
            .context("parent owning context")?;
        // NativePort::observe_model_stream() is true in this host. Its Decisions
        // child does not inherit that caller mode; no media-type inference.
        ensure!(parent_context.context_request_id == parent.as_str() && parent_context.streamed);
        ensure!(parent_context.context_request_id != child_context.context_request_id);
        ensure!(parent_context.request_duration_ms >= child_context.request_duration_ms);
        ensure!(
            parent_scope.operations[0].attempt_ids
                == vec![parent_receipt.attempts[0].attempt_id.clone()]
        );
        ensure!(parent_scope.operations[0].attempt_ids != child_scope.operations[0].attempt_ids);
        ensure!(
            parent_scope.validate_against(&child_receipt).is_err()
                && child_scope.validate_against(&parent_receipt).is_err()
        );
        ensure!(operation_view(&bound, &caller, &parent).await? == parent_scope.operation_view());
        let parent_evidence = usage_evidence(&bound, parent.as_str()).await?;
        let child_evidence = usage_evidence(&bound, child.as_str()).await?;
        parent_evidence.validate()?;
        child_evidence.validate()?;
        ensure!(parent_evidence.execution_id == parent && child_evidence.execution_id == child);
        ensure!(
            parent_evidence.parent_execution_id.is_none()
                && parent_evidence.child_execution_ids == vec![child.clone()]
        );
        ensure!(
            child_evidence.parent_execution_id.as_ref() == Some(&parent)
                && child_evidence.child_execution_ids.is_empty()
        );
        // Parent commit cannot rewrite the separate child's retained object.
        ensure!(scoped(&bound, &caller, &child).await?.0 == child_bytes);
        parent_ack.release.cancel();
        let joined =
            tokio::time::timeout(TIMEOUT, running.as_mut().context("actual native task")?).await?;
        drop(running.take());
        let (session, result) = joined?;
        returned = Some(session);
        let state = result?;
        // Reuse every existing body/usage/identity/edge/page/owned-plan assertion.
        observed(&bound, &state, &sent, false).await?;
        ensure!(scoped(&bound, &caller, &parent).await?.0 == parent_bytes);
        ensure!(scoped(&bound, &caller, &child).await?.0 == child_bytes);
        ensure!(choice.calls() == 1 && generation.calls() == 1);
        ensure!(
            spies.decisions.load(Ordering::SeqCst) == 0
                && spies.transforms.load(Ordering::SeqCst) == 0
        );
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("nested durable getter workload deadline")
    .and_then(|result| result);
    // Release only these fixture barriers, cancel/join the real caller, then
    // original service/socket cleanup. An unknown join cannot become PASS.
    for gate in &release {
        gate.cancel();
    }
    choice.release();
    cancel.cancel();
    let mut cleanup = Vec::new();
    if let Some(mut task) = running.take() {
        match tokio::time::timeout(TIMEOUT, &mut task).await {
            Ok(Ok((session, _))) => returned = Some(session),
            Ok(Err(error)) => cleanup.push(format!("native task join: {error}")),
            Err(error) => {
                task.abort();
                let joined = task.await;
                let joined_ok = joined.is_ok();
                if let Ok((session, _)) = joined {
                    returned = Some(session);
                }
                cleanup.push(format!(
                    "native task deadline: {error}; joined_ok={joined_ok}"
                ));
            }
        }
    }
    if let Some(session) = returned.take()
        && let Err(error) = tokio::time::timeout(TIMEOUT, session.close())
            .await
            .context("native session close deadline")
            .and_then(|result| result.map_err(anyhow::Error::from))
    {
        cleanup.push(error.to_string());
    }
    let choice_closed = choice.finish().await;
    let owners_closed = finish(&bound.fixture, generation, Ok(())).await;
    if let Err(error) = choice_closed {
        cleanup.push(error.to_string());
    }
    if let Err(error) = owners_closed {
        cleanup.push(error.to_string());
    }
    if cleanup.is_empty()
        && let Err(error) = tokio::time::timeout(TIMEOUT, bound.fixture.db.clone().close())
            .await
            .context("nested fixture pool close deadline")
            .and_then(|result| result.map_err(anyhow::Error::from))
    {
        cleanup.push(error.to_string());
    }
    ensure!(
        cleanup.is_empty(),
        "nested fixture closure unconfirmed: {}; workload={checked:?}",
        cleanup.join("; ")
    );
    checked
}
