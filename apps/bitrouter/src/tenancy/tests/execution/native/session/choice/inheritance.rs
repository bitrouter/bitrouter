use super::*;
use crate::storage::checkpoint::snapshot::collaboration::{Action, Work};
use std::collections::BTreeSet;

async fn fixed_receipts(bound: &Bound, state: &SessionSnapshot) -> Result<BTreeSet<String>> {
    let saved = fixture::journal(bound).await?;
    let links = saved["owned_choices"].as_object().context("Choice links")?;
    ensure!(links.len() == state.context_store.decisions.len());
    let mut executions = BTreeSet::new();
    let mut steps = BTreeSet::new();
    tokio::time::timeout(TIMEOUT, bound.fixture.service.admission.wait_drained()).await?;
    for (id, decision) in &state.context_store.decisions {
        let link = links.get(id).context("acknowledged link")?;
        ensure!(
            decision
                .outcome
                .as_ref()
                .is_some_and(|outcome| outcome.is_ok())
                && !decision.stale
        );
        ensure!(link["outcome_digest"].is_string() && link["revision"] == bound.revision.as_str());
        ensure!(
            link["parent"]["resource_id"] == MODEL
                && link["child"]["resource_id"] == fixture::CHILD
        );
        let step_id = link["step_id"].as_str().context("step")?;
        ensure!(steps.insert(step_id.to_owned()));
        let turn = state
            .agents
            .get(&decision.agent_id)
            .context("decision agent")?
            .turn
            .as_ref()
            .context("decision turn")?;
        let step = turn
            .steps
            .iter()
            .find(|step| step.step_id == step_id)
            .context("actual step")?;
        let parent = link["parent_execution"]
            .as_str()
            .context("parent execution")?;
        let child = link["child_execution"]
            .as_str()
            .context("child execution")?;
        ensure!(executions.insert(parent.to_owned()) && executions.insert(child.to_owned()));
        ensure!(
            step.settled
                && step.attempts.len() == 1
                && step
                    .plan
                    .as_ref()
                    .is_some_and(|plan| plan.request_id == parent)
        );
        ensure!(
            step.attempts[0]
                .receipt
                .as_ref()
                .is_some_and(|ack| ack.report.request_id == parent)
        );
        for (execution, model, input, output) in
            [(parent, "model-a", 4, 2), (child, "judge-r1", 7, 3)]
        {
            let receipt = receipt(bound, execution).await?;
            ensure!(
                receipt.execution_revision == bound.revision
                    && receipt.principal_id == bound.fixture.principal
                    && receipt.tenant_id == bound.fixture.tenant
            );
            ensure!(receipt.attempts.len() == 1);
            let attempt = &receipt.attempts[0];
            ensure!(
                attempt
                    .target
                    .as_ref()
                    .is_some_and(|target| target.model_id == model)
            );
            ensure!(
                attempt.outcome == Outcome::Completed
                    && attempt.usage.provenance == Provenance::Provider
                    && attempt.usage.input_tokens == Some(input)
                    && attempt.usage.output_tokens == Some(output)
            );
        }
    }
    let page = page(bound).await?;
    ensure!(page.next_after.is_none() && page.execution_ids.len() == executions.len());
    ensure!(
        page.execution_ids
            .iter()
            .all(|id| executions.contains(id.as_str()))
    );
    Ok(executions)
}

#[tokio::test]
async fn owned_choice_children_and_completion_wakeup_admit_fresh_exact_r1_after_promotion()
-> Result<()> {
    let mut generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::start(true, false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let bound = fixture::sqlite(&generation, &choice, directory.path(), 2).await?;
    let checked = tokio::time::timeout(Duration::from_secs(30), async {
        let session = fixture::open(
            &bound,
            &generation,
            &workspace,
            Arc::new(HostSpies::default()),
        )
        .await?;
        let core = session.core().clone();
        let mut task = fixture::task();
        task.max_concurrent_subagents = Some(1);
        let mut limits = crate::harness::resources::managed::NativeResources::limits();
        limits.active_models = 1;
        task.limits = Some(limits);
        let first = core
            .start("choice-family", core.head().await.state_revision, task)
            .await?;
        let root = first.assigned_ids.get("agent_id").context("root")?.clone();
        let spawned = core
            .collaborate(
                "choice-child",
                core.head().await.state_revision,
                &root,
                Action::Spawn {
                    task: Work {
                        text: "Inherited Choice child".into(),
                        model: None,
                        effort: None,
                        acceptance_criteria: vec![],
                        required_materials: vec![],
                        task_scope: None,
                        fresh_context: false,
                        independent_review: false,
                    },
                },
            )
            .await?;
        let child = spawned
            .assigned_ids
            .get("agent_id")
            .context("child")?
            .clone();
        let running_core = core.clone();
        let running =
            AbortOnDropHandle::new(tokio::spawn(async move { running_core.drive().await }));
        let first_request = choice.receive().await?;
        ensure!(generation.calls() == 0 && !running.is_finished());
        let r2 = draft(&bound.fixture, &bound.route, "choice-promoted", 7).await?;
        result(
            bound
                .fixture
                .service
                .execute(
                    bound.fixture.operator.clone(),
                    Command::Activate(
                        bound.fixture.tenant.clone(),
                        bitrouter_ai::tenancy::configuration::Activation {
                            operation_id: "choice-promote".try_into()?,
                            expected_active_revision: None,
                            revision_id: r2.revision_id,
                            expected_authority_revision: bound
                                .fixture
                                .service
                                .current()
                                .map_err(failure)?
                                .revision,
                            installation_id: bound.fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        choice.release();
        let state = running.await??;
        ensure!(
            state
                .run
                .as_ref()
                .is_some_and(|run| run.status == RunStatus::Completed && run.model_attempts == 3)
        );
        let parent = state.agents.get(&root).context("root state")?;
        let descendant = state.agents.get(&child).context("child state")?;
        ensure!(descendant.parent_id.as_deref() == Some(root.as_str()));
        ensure!(
            parent
                .turn
                .as_ref()
                .is_some_and(|turn| turn.steps.len() == 2)
        );
        ensure!(
            descendant
                .turn
                .as_ref()
                .is_some_and(|turn| turn.steps.len() == 1 && turn.notified)
        );
        ensure!(
            parent
                .mailbox
                .iter()
                .filter(|mail| mail.sender_id == child
                    && mail.kind == "agent_result"
                    && mail.consumed)
                .count()
                == 1
        );
        ensure!(
            state.context_store.decisions.len() == 3
                && choice.calls() == 3
                && generation.calls() == 3
        );
        let executions = fixed_receipts(&bound, &state).await?;
        ensure!(executions.len() == 6);
        for request in [
            first_request,
            choice.receive().await?,
            choice.receive().await?,
        ] {
            ensure!(request.path == "/v1/decisions" && request.body["model"] == "judge-r1");
            ensure!(
                request
                    .headers
                    .get("authorization")
                    .is_some_and(|v| v == "Bearer owned-choice-secret")
            );
            let evidence: Value =
                serde_json::from_str(request.body["input"].as_str().context("input")?)?;
            ensure!(
                state
                    .context_store
                    .decisions
                    .values()
                    .any(|receipt| evidence == json!({"state":receipt.request.state}))
            );
        }
        for _ in 0..3 {
            let sent = generation.receive().await?;
            ensure!(sent.body["model"] == "model-a" && sent.body["max_tokens"] == 25);
            ensure!(
                sent.headers
                    .get("authorization")
                    .is_some_and(|v| v == "Bearer owned-upstream-secret")
            );
        }
        let saved = fixture::journal(&bound).await?;
        let grant = serde_json::from_value(saved["grant"].clone())?;
        core.disconnect().await;
        core.reconnect(&grant, &core.head().await).await?;
        ensure!(core.snapshot().await.context_store.decisions.len() == 3);
        session.close().await?;
        ensure!(
            choice.calls() == 3
                && generation.calls() == 3
                && bound.fixture.service.host_slots_for_test() == 1
        );
        Ok(())
    })
    .await
    .context("Choice family deadline")
    .and_then(|value| value);
    let closed = choice.finish().await;
    finish(&bound.fixture, generation, checked).await?;
    closed
}

#[tokio::test]
async fn owned_choice_uses_real_optional_evidence_without_erasing_source_history() -> Result<()> {
    optional_evidence(bitrouter_ai::core::protocol::ContextMode::Auto).await
}

#[tokio::test]
async fn owned_choice_fixed_context_keeps_history_despite_provider_omission_choice() -> Result<()> {
    optional_evidence(bitrouter_ai::core::protocol::ContextMode::Fixed).await
}

async fn optional_evidence(context: bitrouter_ai::core::protocol::ContextMode) -> Result<()> {
    let mut generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::hiding().await?;
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
        let first = session
            .run(
                fixture::task(),
                Arc::new(Approval),
                CancellationToken::new(),
            )
            .await?;
        ensure!(first.context_store.decisions.len() == 1);
        choice.receive().await?;
        generation.receive().await?;
        let mut next = fixture::task();
        next.text = "A separate task with no use for the prior answer".into();
        next.routing.context = context;
        let second = session
            .run(next, Arc::new(Approval), CancellationToken::new())
            .await?;
        ensure!(
            second
                .run
                .as_ref()
                .is_some_and(|run| run.status == RunStatus::Completed)
        );
        let sent = choice.receive().await?;
        let generation_request = generation.receive().await?;
        let run = second.run.as_ref().context("second run")?;
        let decision = second
            .context_store
            .decisions
            .values()
            .find(|entry| entry.run_id == run.run_id)
            .context("second decision")?;
        let outcome = decision.outcome.as_ref().context("Choice outcome ACK")?;
        ensure!(
            outcome.is_ok(),
            "Choice outcome failed: {:?}",
            outcome.as_ref().err().map(|error| &error.kind)
        );
        ensure!(decision.policy.minimum_savings_fraction == 0.0);
        ensure!(
            !decision.candidates.candidates.is_empty(),
            "test never offered optional evidence"
        );
        ensure!(
            sent.body["questions"]
                .as_array()
                .is_some_and(|items| items.iter().any(|question| question["name"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("evidence_"))))
        );
        let view = second
            .context_store
            .views
            .values()
            .find(|view| view.decision_id.as_deref() == Some(&decision.decision_id))
            .context("applied Choice view")?;
        let automatic = context == bitrouter_ai::core::protocol::ContextMode::Auto;
        ensure!(
            view.omitted.is_empty() != automatic,
            "selected context did not respect the caller's omission capability: reason={}",
            view.reason
        );
        ensure!(
            view.reason
                == if automatic {
                    "typed_context_decision"
                } else {
                    "full_context_candidate"
                }
        );
        for block_id in &view.omitted {
            let source = second
                .context_store
                .evidence
                .get(block_id)
                .context("retained source")?;
            ensure!(!source.protected && !decision.candidates.required.contains(block_id));
            ensure!(
                source
                    .messages
                    .iter()
                    .any(|message| message.role == bitrouter_ai::types::Role::Assistant)
            );
            source.validate()?;
        }
        ensure!(
            generation_request.body["messages"]
                .as_array()
                .context("generation history")?
                .iter()
                .any(|message| message["role"] == "assistant"
                    && message["content"] == "native reply")
                != automatic
        );
        ensure!(
            second
                .agents
                .get(&second.agent_id)
                .context("root")?
                .history
                .iter()
                .filter(|message| message.role == bitrouter_ai::types::Role::Assistant)
                .count()
                >= 2,
            "view selection erased canonical source history"
        );
        ensure!(choice.calls() == 2 && generation.calls() == 2);
        session.close().await?;
        Ok(())
    })
    .await
    .context("Choice optional evidence deadline")
    .and_then(|value| value);
    let closed = choice.finish().await;
    finish(&bound.fixture, generation, checked).await?;
    closed
}
