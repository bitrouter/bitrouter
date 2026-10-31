//! Actual native facade, separate Decisions socket and independent receipt/ACK evidence.
use super::*;
use crate::storage::checkpoint::snapshot::{RunStatus, SessionSnapshot};
use crate::tenancy::receipts::read;
use bitrouter_ai::tenancy::execution::{ExecutionPage, ExecutionReceipt, Outcome, Provenance};
use fixture::Bound;
mod authority;
#[cfg(unix)]
pub(super) mod custody;
mod fallback;
pub(in crate::tenancy::tests::execution::native) mod fixture;
#[cfg(bitrouter_capacity_tests)]
mod headroom;
mod inheritance;
mod lifetime;
#[cfg(unix)]
mod readback;
#[cfg(unix)]
mod recovery;
mod server;
pub(in crate::tenancy::tests::execution::native) mod wire;

async fn receipt(bound: &Bound, id: &str) -> Result<ExecutionReceipt> {
    let bytes = bound
        .fixture
        .service
        .read_receipt(
            bound.fixture.operator.clone(),
            bound.fixture.tenant.clone(),
            read::Request::One(id.try_into()?),
        )
        .await
        .map_err(failure)?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn usage_evidence(
    bound: &Bound,
    id: &str,
) -> Result<bitrouter_ai::tenancy::usage_evidence::ExecutionUsageEvidenceV1> {
    let bytes = bound
        .fixture
        .service
        .read_receipt(
            bound.fixture.operator.clone(),
            bound.fixture.tenant.clone(),
            read::Request::UsageEvidence(id.try_into()?),
        )
        .await
        .map_err(failure)?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn page(bound: &Bound) -> Result<ExecutionPage> {
    let bytes = bound
        .fixture
        .service
        .read_receipt(
            bound.fixture.operator.clone(),
            bound.fixture.tenant.clone(),
            read::Request::Page {
                correlation: "same-correlation".try_into()?,
                after: None,
                limit: 100,
            },
        )
        .await
        .map_err(failure)?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn observed(
    bound: &Bound,
    state: &SessionSnapshot,
    sent: &Observed,
    invalid: bool,
) -> Result<()> {
    ensure!(state.run.as_ref().context("run")?.status == RunStatus::Completed);
    ensure!(state.context_store.decisions.len() == 1);
    let decision = state
        .context_store
        .decisions
        .values()
        .next()
        .context("committed Choice outcome")?;
    ensure!(!decision.stale && decision.request.model == fixture::CHILD);
    let source: Value =
        serde_json::from_str(sent.body["input"].as_str().context("native evidence")?)?;
    ensure!(source == json!({"state":decision.request.state}));
    let names = sent.body["questions"]
        .as_array()
        .context("native questions")?
        .iter()
        .map(|question| question["name"].as_str().context("question name"))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        names
            == decision
                .request
                .questions
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
    );
    ensure!(names.len() == 3 && names.iter().all(|name| name.starts_with("routing_")));
    ensure!(sent.path == "/v1/decisions" && sent.body["model"] == "judge-r1");
    ensure!(
        sent.headers
            .get("authorization")
            .is_some_and(|value| value == "Bearer owned-choice-secret")
    );
    let result = decision.outcome.as_ref().context("Choice outcome ACK")?;
    if invalid {
        let failure = result
            .as_ref()
            .err()
            .context("invalid confidence accepted")?;
        ensure!(
            failure.kind
                == crate::gateway::model::decision::types::DecisionFailure::InvalidResponse
                && failure.may_have_run
                && failure
                    .usage
                    .is_some_and(|usage| usage.input_tokens == 7 && usage.output_tokens == 3)
        );
        ensure!(
            state
                .context_store
                .views
                .values()
                .all(|view| view.omitted.is_empty())
        );
    } else {
        let answer = result
            .as_ref()
            .map_err(|error| anyhow::anyhow!("Choice rejected: {error}"))?;
        ensure!(
            answer.model == "judge-reported-r1"
                && answer.usage.input_tokens == 7
                && answer.usage.output_tokens == 3
        );
    }
    let saved = fixture::journal(bound).await?;
    let links = saved["owned_choices"]
        .as_object()
        .context("same-row Choice links")?;
    ensure!(links.len() == 1);
    let link = links
        .get(&decision.decision_id)
        .context("same-row intent link")?;
    ensure!(
        link["outcome_digest"]
            .as_str()
            .is_some_and(|digest| digest.len() == 64)
    );
    ensure!(link["revision"] == bound.revision.as_str());
    ensure!(
        link["installation"] == bound.fixture.installation.as_str()
            && link["tenant"] == bound.fixture.tenant.as_str()
    );
    let parent = link["parent_execution"]
        .as_str()
        .context("real parent ID")?;
    let child = link["child_execution"].as_str().context("real child ID")?;
    ensure!(parent != child);
    let turn = state
        .agents
        .get(&state.agent_id)
        .context("root")?
        .turn
        .as_ref()
        .context("turn")?;
    ensure!(turn.steps.len() == 1 && turn.steps[0].settled && turn.steps[0].attempts.len() == 1);
    let step = &turn.steps[0];
    let plan = step.plan.as_ref().context("actual Generation plan")?;
    ensure!(
        plan.request_id == parent && plan.routes.len() == 1 && plan.routes[0].model == "model-a"
    );
    ensure!(
        step.attempts[0]
            .receipt
            .as_ref()
            .is_some_and(|ack| ack.report.request_id == parent)
    );
    // The context decision has its own IDs and usage; it is never a Generation attempt.
    tokio::time::timeout(TIMEOUT, bound.fixture.service.admission.wait_drained()).await?;
    let child_receipt = receipt(bound, child).await?;
    let parent_receipt = receipt(bound, parent).await?;
    let parent_evidence = usage_evidence(bound, parent).await?;
    let child_evidence = usage_evidence(bound, child).await?;
    ensure!(
        parent_evidence.parent_execution_id.is_none()
            && parent_evidence.child_execution_ids == vec![child_receipt.execution_id.clone()]
            && child_evidence.parent_execution_id.as_ref() == Some(&parent_receipt.execution_id)
            && child_evidence.child_execution_ids.is_empty(),
        "owned Choice linkage must come from actual admission in both receipts"
    );
    for (evidence, receipt) in [
        (&parent_evidence, &parent_receipt),
        (&child_evidence, &child_receipt),
    ] {
        ensure!(
            evidence.capture == bitrouter_ai::tenancy::usage_evidence::Capture::Captured
                && evidence.installation_id == receipt.installation_id
                && evidence.tenant_id == receipt.tenant_id
                && evidence.execution_id == receipt.execution_id
                && evidence.attempts.len() == receipt.attempts.len()
                && !evidence.frames.is_empty()
        );
        ensure!(
            evidence.frames.iter().all(|frame| receipt
                .attempts
                .iter()
                .any(|attempt| attempt.attempt_id == frame.attempt_id)),
            "child observations must remain in their own execution, without copying or summing into the parent"
        );
    }
    for (value, model, input, output) in [
        (&child_receipt, "judge-r1", 7, 3),
        (&parent_receipt, "model-a", 4, 2),
    ] {
        ensure!(
            value.execution_revision == bound.revision
                && value.tenant_id == bound.fixture.tenant
                && value.principal_id == bound.fixture.principal
                && value.installation_id == bound.fixture.installation
        );
        ensure!(value.attempts.len() == 1);
        let attempt = &value.attempts[0];
        ensure!(
            attempt
                .target
                .as_ref()
                .is_some_and(|target| target.model_id == model)
        );
        ensure!(
            attempt.usage.provenance == Provenance::Provider
                && attempt.usage.input_tokens == Some(input)
                && attempt.usage.output_tokens == Some(output)
        );
        // Finite native confidence 2.0 is accepted by that wire but rejected
        // by Core Choice. This cannot rewrite original completed provider work.
        ensure!(attempt.outcome == Outcome::Completed);
    }
    ensure!(parent_receipt.attempts[0].attempt_id != child_receipt.attempts[0].attempt_id);
    let page = page(bound).await?;
    ensure!(page.multiple_executions && page.next_after.is_none() && page.execution_ids.len() == 2);
    ensure!(
        page.execution_ids.contains(&child_receipt.execution_id)
            && page.execution_ids.contains(&parent_receipt.execution_id)
    );
    Ok(())
}

pub(in crate::tenancy::tests::execution::native) async fn positive(
    bound: &Bound,
    generation: &mut Provider,
    choice: &mut wire::Endpoint,
    workspace: &std::path::Path,
    invalid: bool,
) -> Result<()> {
    let spies = Arc::new(HostSpies::default());
    let mut session = fixture::open(bound, generation, workspace, spies.clone()).await?;
    let core = session.core().clone();
    ensure!(
        core.snapshot()
            .await
            .manifest
            .required_features
            .iter()
            .any(|value| value == "context_views_v1")
    );
    let mut running = AbortOnDropHandle::new(tokio::spawn(async move {
        let result = session
            .run(
                fixture::task(),
                Arc::new(Approval),
                CancellationToken::new(),
            )
            .await;
        (session, result)
    }));
    let sent = tokio::select! {
        received = choice.receive() => match received {
            Ok(sent) => sent,
            Err(error) => {
                let state = core.snapshot().await;
                anyhow::bail!("Choice request was not observed: {error:#}; run={:?}; root={:?}; decisions={}",
                    state.run.as_ref().map(|run| (&run.status, &run.terminal_reason, &run.resource_error)),
                    state.root_turn().map(|turn| (&turn.status, &turn.terminal_reason, turn.steps.last().and_then(|step| step.failure_diagnostic.as_ref()))),
                    state.context_store.decisions.len());
            }
        },
        completed = &mut running => {
            let (session, result) = completed?;
            let state = core.snapshot().await;
            let closed = tokio::time::timeout(TIMEOUT, session.close()).await;
            anyhow::bail!("Native task ended before Choice request: error={:?}; run={:?}; root={:?}; decisions={}; close={closed:?}",
                result.err(),
                state.run.as_ref().map(|run| (&run.status, &run.terminal_reason, &run.resource_error)),
                state.root_turn().map(|turn| (&turn.status, &turn.terminal_reason, turn.steps.last().and_then(|step| step.failure_diagnostic.as_ref()))),
                    state.context_store.decisions.len());
        }
    };
    let pending = core.snapshot().await;
    ensure!(
        pending.context_store.decisions.len() == 1
            && pending
                .context_store
                .decisions
                .values()
                .all(|entry| entry.outcome.is_none())
    );
    let saved = fixture::journal(bound).await?;
    ensure!(
        saved["owned_choices"]
            .as_object()
            .is_some_and(|links| links.len() == 1)
    );
    let (_, _, active, queued) = bound
        .fixture
        .service
        .admission
        .ingress_for_test()
        .map_err(|code| anyhow::anyhow!("admission witness: {code:?}"))?;
    ensure!(active == 2 && queued == 0 && generation.calls() == 0 && !running.is_finished());
    ensure!(
        core.release("early-release", core.head().await.state_revision)
            .await
            .is_err()
    );
    choice.release();
    let (session, result) = tokio::time::timeout(TIMEOUT, running).await??;
    let state = result?;
    let generation_request = generation.receive().await?;
    ensure!(
        generation_request.path == "/v1/chat/completions"
            && generation_request.body["model"] == "model-a"
            && generation_request.body["max_tokens"] == 25
    );
    observed(bound, &state, &sent, invalid).await?;
    ensure!(choice.calls() == 1 && generation.calls() == 1);
    ensure!(
        spies.decisions.load(Ordering::SeqCst) == 0 && spies.transforms.load(Ordering::SeqCst) == 0
    );
    tokio::time::timeout(TIMEOUT, session.close()).await??;
    ensure!(fixture::journal(bound).await?["released"] == true);
    Ok(())
}

#[tokio::test]
async fn owned_choice_capacity_two_acks_real_intent_before_send_and_preserves_independent_receipt()
-> Result<()> {
    for invalid in [false, true] {
        let mut generation = Provider::start(false).await?;
        let mut choice = wire::Endpoint::start(true, invalid).await?;
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let bound = fixture::sqlite(&generation, &choice, directory.path(), 2).await?;
        let checked = tokio::time::timeout(
            Duration::from_secs(25),
            positive(&bound, &mut generation, &mut choice, &workspace, invalid),
        )
        .await
        .context("owned Choice positive deadline")
        .and_then(|result| result);
        let choice_closed = choice.finish().await;
        let finished = finish(&bound.fixture, generation, checked).await;
        finished?;
        choice_closed?;
    }
    Ok(())
}

#[tokio::test]
async fn owned_choice_capacity_one_fails_before_child_queue_or_provider() -> Result<()> {
    let generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::start(false, false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let bound = fixture::sqlite(&generation, &choice, directory.path(), 1).await?;
    let checked = tokio::time::timeout(TIMEOUT, async {
        let mut session = fixture::open(
            &bound,
            &generation,
            &workspace,
            Arc::new(HostSpies::default()),
        )
        .await?;
        let state = session
            .run(
                fixture::task(),
                Arc::new(Approval),
                CancellationToken::new(),
            )
            .await?;
        ensure!(
            state
                .manifest
                .required_features
                .iter()
                .any(|value| value == "context_views_v1")
        );
        ensure!(
            !state.context_store.work.is_empty() && !state.context_store.evidence.is_empty(),
            "did not enter actual context preparation"
        );
        ensure!(
            state
                .run
                .as_ref()
                .is_some_and(|run| run.status == RunStatus::Failed)
        );
        let turn = state
            .agents
            .get(&state.agent_id)
            .context("root")?
            .turn
            .as_ref()
            .context("turn")?;
        ensure!(
            turn.terminal_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("LimitExceeded")),
            "not the child capacity rejection"
        );
        ensure!(
            turn.steps.len() == 1 && turn.steps[0].settled && turn.steps[0].attempts.is_empty()
        );
        ensure!(
            state.context_store.decisions.is_empty()
                && choice.calls() == 0
                && generation.calls() == 0
        );
        ensure!(bound.fixture.service.admission.queued_for_test() == 0);
        session.close().await?;
        tokio::time::timeout(TIMEOUT, bound.fixture.service.admission.wait_drained()).await?;
        let receipts = page(&bound).await?;
        ensure!(receipts.execution_ids.len() == 1 && !receipts.multiple_executions);
        ensure!(
            receipt(&bound, receipts.execution_ids[0].as_str())
                .await?
                .attempts
                .is_empty()
        );
        Ok(())
    })
    .await
    .context("fail-fast Choice capacity deadline")
    .and_then(|result| result);
    let choice_closed = choice.finish().await;
    finish(&bound.fixture, generation, checked).await?;
    choice_closed
}
