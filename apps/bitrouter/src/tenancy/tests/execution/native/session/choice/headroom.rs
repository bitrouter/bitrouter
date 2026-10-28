//! Receipt sub-budgets through the actual native facade, driver and journal ACK.
use super::*;
use crate::storage::checkpoint::snapshot::collaboration::{Action, Work};

const RECEIPTS: usize = 8 * 1024 * 1024;
const RESPONSE: usize = 2 * 128 * 1024 + 8192;
// Up to forty real turns repeatedly serialize a growing 8 MiB receipt history.
// Debug sampling showed checkpoint serialization progressing at the former
// two-minute campaign deadline; individual operation deadlines stay unchanged.
const CAMPAIGN: Duration = Duration::from_secs(30 * 60);
// At the actual byte frontier, preparation serializes the retained journal before
// a socket is observable. This test waiter covers the original request interval;
// it never changes any execution, callback, cleanup or actor deadline.
const OBSERVATION: Duration = Duration::from_secs(60);

// A deadline failure reports bounded counters, never journal or provider content.
async fn progress(core: &crate::core::session::CoreSession, result: Result<()>) -> Result<()> {
    if result.is_ok() {
        return result;
    }
    match tokio::time::timeout(TIMEOUT, core.snapshot()).await {
        Ok(state) => {
            let complete = state
                .context_store
                .decisions
                .values()
                .filter(|receipt| receipt.outcome.is_some())
                .count();
            let retained = bytes(&state)?;
            result.with_context(|| format!("Choice capacity progress: decisions={}, completed={complete}, receipt_bytes={retained}, run={:?}",
                state.context_store.decisions.len(), state.run.as_ref().map(|run| &run.status)))
        }
        Err(_) => result.context("Choice capacity progress snapshot exceeded its deadline"),
    }
}

fn task(index: usize) -> TaskInput {
    let mut task = fixture::task();
    task.text = format!("Receipt capacity turn {index}");
    task.limits = Some(crate::harness::resources::managed::NativeResources::limits());
    task
}

fn bytes(state: &SessionSnapshot) -> Result<usize> {
    state
        .context_store
        .decisions
        .values()
        .try_fold(0_usize, |sum, receipt| {
            sum.checked_add(serde_json::to_vec(receipt)?.len())
                .context("receipt byte total overflow")
        })
}

async fn successful_turn(
    session: &mut NativeSession,
    generation: &mut Provider,
    choice: &mut wire::Endpoint,
    index: usize,
) -> Result<SessionSnapshot> {
    let before = (generation.calls(), choice.calls());
    let state = session
        .run(task(index), Arc::new(Approval), CancellationToken::new())
        .await?;
    ensure!(state.run.as_ref().context("run")?.status == RunStatus::Completed);
    ensure!(generation.calls() == before.0 + 1 && choice.calls() == before.1 + 1);
    ensure!(choice.receive().await?.body["model"] == "judge-r1");
    ensure!(generation.receive().await?.body["model"] == "model-a");
    let newest = state
        .context_store
        .decisions
        .values()
        .find(|receipt| {
            state
                .run
                .as_ref()
                .is_some_and(|run| run.run_id == receipt.run_id)
        })
        .context("current real Choice receipt")?;
    let response = newest
        .outcome
        .as_ref()
        .context("outcome ACK")?
        .as_ref()
        .map_err(|error| anyhow::anyhow!("large bounded answer rejected: {error}"))?;
    ensure!(response.model.len() > 240 * 1024 && !newest.stale);
    ensure!(
        serde_json::to_vec(newest.outcome.as_ref().context("outcome")?)?.len()
            <= newest.response_limit_bytes
    );
    Ok(state)
}

#[tokio::test]
async fn owned_choice_receipt_capacity_refuses_before_socket_send_and_keeps_clean_release()
-> Result<()> {
    let mut generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::large().await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let bound = fixture::sqlite(&generation, &choice, directory.path(), 2).await?;
    let mut session = fixture::open(
        &bound,
        &generation,
        &workspace,
        Arc::new(HostSpies::default()),
    )
    .await?;
    let checked = tokio::time::timeout(CAMPAIGN, async {
        let mut accepted = 0;
        for index in 0..40 {
            let before = session.core().snapshot().await;
            let calls = (generation.calls(), choice.calls());
            let state = session
                .run(task(index), Arc::new(Approval), CancellationToken::new())
                .await?;
            if state.run.as_ref().context("run")?.status == RunStatus::Completed {
                accepted += 1;
                ensure!(choice.calls() == calls.1 + 1 && generation.calls() == calls.0 + 1);
                choice.receive().await?;
                generation.receive().await?;
                let newest = state
                    .context_store
                    .decisions
                    .values()
                    .find(|receipt| {
                        state
                            .run
                            .as_ref()
                            .is_some_and(|run| run.run_id == receipt.run_id)
                    })
                    .context("new receipt")?;
                ensure!(newest.outcome.as_ref().is_some_and(|result| {
                    result
                        .as_ref()
                        .is_ok_and(|answer| answer.model.len() > 240 * 1024)
                }));
                continue;
            }
            ensure!(
                accepted >= 20,
                "did not reach the real receipt byte boundary"
            );
            ensure!(bytes(&before)? > RECEIPTS - 2 * RESPONSE && bytes(&state)? <= RECEIPTS);
            ensure!(state.run.as_ref().context("run")?.status == RunStatus::Failed);
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
                "capacity was misclassified as unknown commit or malformed linkage"
            );
            ensure!(
                choice.calls() == calls.1 && generation.calls() == calls.0,
                "capacity rejection reached a provider socket"
            );
            ensure!(
                serde_json::to_value(&before.context_store.decisions)?
                    == serde_json::to_value(&state.context_store.decisions)?,
                "a refused intent changed retained paid history"
            );
            ensure!(
                turn.steps.len() == 1 && turn.steps[0].settled && turn.steps[0].attempts.is_empty()
            );
            tokio::time::timeout(TIMEOUT, bound.fixture.service.admission.wait_drained()).await?;
            let observed = page(&bound).await?;
            ensure!(observed.next_after.is_none());
            let mut attempts = 0;
            let mut empty = 0;
            for id in observed.execution_ids {
                let execution = receipt(&bound, id.as_str()).await?;
                attempts += execution.attempts.len();
                if execution.attempts.is_empty() {
                    empty += 1;
                }
            }
            // The actual parent and fail-fast child were admitted, but neither
            // fabricated a provider attempt when the native journal refused intent.
            ensure!(attempts == 2 * accepted && empty == 2);
            ensure!(bound.fixture.service.current().is_ok());
            return Ok(());
        }
        anyhow::bail!("bounded receipt campaign never reached capacity")
    })
    .await
    .context("receipt headroom campaign deadline")
    .and_then(|result| result);
    let checked = progress(session.core(), checked).await;
    let closed = tokio::time::timeout(TIMEOUT, session.close()).await;
    let endpoint = choice.finish().await;
    let finished = finish(&bound.fixture, generation, checked).await;
    finished?;
    closed.context("native release deadline")??;
    endpoint?;
    bound.fixture.service.cleanup.ensure_clean()?;
    ensure!(bound.fixture.service.host_slots_for_test() == 1);
    ensure!(fixture::journal(&bound).await?["released"] == true);
    Ok(())
}

#[tokio::test]
async fn owned_choice_two_pending_intents_keep_both_outcome_acks_near_receipt_capacity()
-> Result<()> {
    let mut generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::large().await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let bound = fixture::sqlite(&generation, &choice, directory.path(), 4).await?;
    let mut session = fixture::open(
        &bound,
        &generation,
        &workspace,
        Arc::new(HostSpies::default()),
    )
    .await?;
    let checked = tokio::time::timeout(CAMPAIGN, async {
        let mut state = session.core().snapshot().await;
        for index in 0..40 {
            if bytes(&state)? >= RECEIPTS - 3 * RESPONSE {
                break;
            }
            state = successful_turn(&mut session, &mut generation, &mut choice, index).await?;
        }
        ensure!(bytes(&state)? >= RECEIPTS - 3 * RESPONSE);
        let prior = state.context_store.decisions.len();
        let core = session.core().clone();
        let mut input = task(100);
        input.max_concurrent_subagents = Some(1);
        input.limits.as_mut().context("limits")?.active_models = 2;
        let started = core
            .start("headroom-family", core.head().await.state_revision, input)
            .await?;
        let root = started
            .assigned_ids
            .get("agent_id")
            .context("root")?
            .clone();
        core.collaborate(
            "headroom-child",
            core.head().await.state_revision,
            &root,
            Action::Spawn {
                task: Work {
                    text: "Second concurrent retained Choice".into(),
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
        let sent_before = choice.calls();
        choice.pause_next();
        let driver = core.clone();
        let sockets_deadline = tokio::time::Instant::now() + OBSERVATION;
        let running = AbortOnDropHandle::new(tokio::spawn(async move { driver.drive().await }));
        choice.receive_until(sockets_deadline).await?;
        choice.receive_until(sockets_deadline).await?;
        let pending = core.snapshot().await;
        let ids = pending
            .context_store
            .decisions
            .iter()
            .filter(|(_, receipt)| receipt.outcome.is_none())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        ensure!(ids.len() == 2 && pending.context_store.decisions.len() == prior + 2);
        ensure!(choice.calls() == sent_before + 2 && !running.is_finished());
        // Independent serialization oracle over the two REAL acknowledged intents.
        // This is a stopped-owner terminal projection, not a claim that the live
        // owner stopped and not a recovery or replay API.
        let mut stopped = pending.clone();
        for id in &ids {
            let receipt = stopped
                .context_store
                .decisions
                .get_mut(id)
                .context("pending receipt")?;
            receipt.outcome = Some(Err(crate::gateway::model::decision::types::DecisionError {
                kind: crate::gateway::model::decision::types::DecisionFailure::Interrupted,
                message: "previous owner stopped before recording the decision outcome".into(),
                usage: None,
                may_have_run: true,
            }));
            ensure!(receipt.elapsed_ms.is_none());
            ensure!(serde_json::to_vec(receipt)?.len() <= 1024 * 1024);
        }
        ensure!(bytes(&stopped)? <= RECEIPTS);
        let saved = fixture::journal(&bound).await?;
        for id in &ids {
            ensure!(saved["owned_choices"][id]["outcome_digest"].is_null());
        }
        ensure!(
            bound
                .fixture
                .service
                .admission
                .ingress_for_test()
                .map_err(|code| anyhow::anyhow!("admission: {code:?}"))?
                .2
                == 4
        );
        choice.resume();
        let state = tokio::time::timeout(OBSERVATION, running).await???;
        ensure!(bytes(&state)? <= RECEIPTS);
        let saved = fixture::journal(&bound).await?;
        for id in &ids {
            let result = state.context_store.decisions[id]
                .outcome
                .as_ref()
                .context("both Core outcomes ACKed")?;
            ensure!(
                result
                    .as_ref()
                    .is_ok_and(|answer| answer.model.len() > 240 * 1024)
            );
            ensure!(
                saved["owned_choices"][id]["outcome_digest"]
                    .as_str()
                    .is_some_and(|digest| digest.len() == 64)
            );
            let child = saved["owned_choices"][id]["child_execution"]
                .as_str()
                .context("child execution")?;
            let execution = receipt(&bound, child).await?;
            ensure!(
                execution.attempts.len() == 1
                    && execution.attempts[0].usage.provenance == Provenance::Provider
            );
        }
        ensure!(
            state
                .context_store
                .decisions
                .values()
                .all(|receipt| receipt.outcome.is_some())
        );
        ensure!(bound.fixture.service.current().is_ok());
        Ok(())
    })
    .await
    .context("two-pending headroom deadline")
    .and_then(|result| result);
    choice.resume();
    let checked = progress(session.core(), checked).await;
    let closed = tokio::time::timeout(TIMEOUT, session.close()).await;
    let endpoint = choice.finish().await;
    let finished = finish(&bound.fixture, generation, checked).await;
    finished?;
    closed.context("native release deadline")??;
    endpoint?;
    bound.fixture.service.cleanup.ensure_clean()?;
    ensure!(bound.fixture.service.host_slots_for_test() == 1);
    Ok(())
}
