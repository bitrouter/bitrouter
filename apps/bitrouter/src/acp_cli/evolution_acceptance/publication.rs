//! Controlled real-worker publication: no planted adoption or learner rows.
//! All feedback is produced by the local judge fixture over actual ACP capture.

use super::*;
use std::collections::BTreeSet;

use crate::evolution::bandit::Arm;
use crate::evolution::control::BlockStatus;
use crate::evolution::learning::LearningReport;
use crate::evolution::service::SessionEnrollment;

const GOOD: &str = "Reply exactly ACP_ACCEPTANCE_OK. Do not use tools, run tests, request a review or create a PR.";
const BAD: &str = "Reply exactly ACP_ACCEPTANCE_OK. Do not use tools, run tests, request a review or create a PR. Controlled feedback marker: ACP_ACCEPTANCE_REJECTED.";

fn model_profile(request: &Request) -> Result<ResponseTemplate> {
    let body: Value = serde_json::from_slice(&request.body)?;
    // Known synthetic prices and latency establish a useful candidate for the
    // serving mechanics test. Neither these profiles nor the planted rubric
    // labels are measurements of actual model quality or speed.
    let delay = match body["model"].as_str() {
        Some("strong") => 250,
        Some("cheap") => 5,
        _ => 0,
    };
    Ok(response(request)?.set_delay(Duration::from_millis(delay)))
}

async fn resources_ready(fixture: &Fixture, identity: &SessionIdentity) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let attempt = async {
                let head = fixture.canonical.transcript(identity).await?.session.head;
                let checkpoint = fixture.canonical.freeze_checkpoint(identity, head).await?;
                fixture
                    .canonical
                    .observe_checkpoint_resources(identity, &checkpoint.checkpoint_id)
                    .await
            }
            .await;
            match attempt {
                Ok(resource) if resource.metering_complete && !resource.requests.is_empty() => {
                    return Ok(());
                }
                Ok(_) => {}
                Err(error)
                    if error.to_string().contains("changed")
                        || error.to_string().contains("retry") => {}
                Err(error) => return Err(error),
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("worker requests did not settle before shutdown")?
}

async fn episode(
    fixture: &Fixture,
    agent: &str,
    prompt: &str,
) -> Result<(SessionIdentity, SessionEnrollment)> {
    // One live worker at a time avoids retaining a process for every native
    // session. Each fresh controller still shares the real persisted learner.
    let mut handle = launch(fixture, agent).await?;
    let identity = SessionIdentity {
        owner: "local".into(),
        source: agent.into(),
        native_session_id: handle.session_id.clone(),
    };
    let outcome = async {
        let response = handle.client.prompt(&handle.session_id, prompt).await?;
        ensure!(
            response.stop_reason == agent_client_protocol::schema::v1::StopReason::EndTurn,
            "publication fixture did not finish its native turn"
        );
        resources_ready(fixture, &identity).await
    }
    .await;
    let clean = handle.shutdown().await;
    outcome?;
    ensure!(clean, "publication worker cleanup was not confirmed");
    let enrollment = fixture
        .runtime
        .service("local")?
        .enrollment(&identity)
        .await?
        .context("native session enrollment missing")?;
    Ok((identity, enrollment))
}

async fn evaluate(fixture: &Fixture, identity: &SessionIdentity) -> Result<LearningReport> {
    let service = fixture.runtime.service("local")?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let report = EvolutionScheduler::new(fixture.runtime.clone())
                .tick(&fixture.pipeline)
                .await?;
            ensure!(
                report.errors.values().all(|reason| matches!(
                    reason.as_str(),
                    "checkpoint_discovery_retry_required"
                        | "resource_refresh_retry_required"
                        | "learning_or_route_validation_retry_required"
                )),
                "publication scheduler errors: {:?}",
                report.errors
            );
            let effective = fixture.canonical.effective_assessment(identity).await?;
            if report.errors.is_empty()
                && !effective.stale
                && effective.assessment.is_some()
                && effective
                    .resource
                    .as_ref()
                    .is_some_and(|resource| resource.metering_complete)
            {
                let learning = service.learning_status("acceptance").await?;
                ensure!(
                    !effective.source_capture_states.is_empty()
                        && effective
                            .source_capture_states
                            .values()
                            .all(|state| state == "complete"),
                    "native capture did not close cleanly"
                );
                let resource = effective.resource.context("completed resources missing")?;
                let executions = fixture.runtime.executions(identity).await?;
                let actual: BTreeSet<_> =
                    executions.iter().map(|e| e.request_id.as_str()).collect();
                let included: BTreeSet<_> = resource
                    .requests
                    .iter()
                    .map(|r| r.request_id.as_str())
                    .collect();
                let known = executions.iter().try_fold(0_u64, |total, execution| {
                    let cost = execution
                        .settlement
                        .as_ref()
                        .and_then(|s| s.total_cost_micro_usd)
                        .context("actual request cost missing")?;
                    total.checked_add(cost).context("actual cost overflow")
                })?;
                ensure!(
                    !actual.is_empty()
                        && actual == included
                        && actual.len() == executions.len()
                        && included.len() == resource.requests.len()
                        && resource.unassigned_request_ids.is_empty()
                        && resource.known_cost_micro_usd == i64::try_from(known)?,
                    "scored resources differ from the closed native request union"
                );
                let key = identity.key()?;
                if let Some(observation) = learning
                    .observations
                    .sessions
                    .get(&key)
                    .or_else(|| learning.monitoring_observations.sessions.get(&key))
                {
                    ensure!(
                        observation.total_cost_micro_usd == Some(known),
                        "learner cost differs from actual native execution cost"
                    );
                }
                return Ok(learning);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("completed native session did not enter learning")?
}

async fn assert_model(fixture: &Fixture, identity: &SessionIdentity, expected: &str) -> Result<()> {
    let executions = fixture.runtime.executions(identity).await?;
    ensure!(
        !executions.is_empty(),
        "native worker made no model request"
    );
    let mut matched = false;
    for execution in executions {
        let settled = execution
            .settlement
            .as_ref()
            .context("actual execution settlement missing")?;
        let selected = settled.final_model == expected;
        // A policy includes its request capability guard. Auxiliary worker
        // requests can require capabilities the cheap fixture does not offer;
        // keep and charge that baseline work under the original trial arm.
        let guarded = expected == "cheap"
            && settled.final_model == "strong"
            && execution.route_guard_reason.as_deref()
                == Some("candidate_request_capability_unverified")
            && execution.intent.as_ref().is_some_and(|intent| {
                intent.selected_route == "candidate" && intent.bypass.is_none()
            });
        ensure!(
            (selected || guarded)
                && settled.final_provider == "fixture"
                && settled.total_cost_micro_usd.is_some(),
            "native worker expected {expected}, execution: {execution:?}"
        );
        matched |= selected;
    }
    ensure!(
        matched,
        "native trial never actually dispatched its {expected} model"
    );
    Ok(())
}

async fn run_publication(agent: &str, adapter: &str, worker: &str) -> Result<()> {
    let fixture = fixture_with_response(agent, adapter, worker, model_profile).await?;
    configure(&fixture, agent).await?;
    let service = fixture.runtime.service("local")?;
    let mut sessions = Vec::new();
    let mut baseline = 0;
    let mut challenger = 0;
    let mut adopted = None;
    // Keep the product's quality, sample, latency and publication gates. The
    // fixture's initial 50% exposure and known model profiles are explicit.
    for arrival in 1..=192 {
        let (identity, enrollment) = episode(&fixture, agent, GOOD).await?;
        let assignment = enrollment
            .assignments
            .get("acceptance")
            .context("fresh trial assignment missing")?;
        let expected = match assignment.arm {
            Arm::Baseline => {
                baseline += 1;
                "strong"
            }
            Arm::Challenger => {
                challenger += 1;
                "cheap"
            }
        };
        assert_model(&fixture, &identity, expected).await?;
        let learning = evaluate(&fixture, &identity).await?;
        sessions.push(identity);
        ensure!(
            learning.observations.sessions.len() == sessions.len(),
            "native trials were duplicated or lost"
        );
        ensure!(
            learning.block_status != BlockStatus::RolledBack,
            "controlled useful candidate was withdrawn: {:?}",
            learning.plan
        );
        if arrival % 8 == 0 || learning.block_status == BlockStatus::Adopted {
            eprintln!(
                "{agent}: trial {arrival}; baseline {baseline}; challenger {challenger}; status {:?}; benefit lower {} ppm",
                learning.block_status, learning.plan.monte_carlo_lower_ppm
            );
        }
        if learning.block_status == BlockStatus::Adopted {
            ensure!(
                baseline >= 20 && challenger >= 20,
                "promotion bypassed default family requirements"
            );
            adopted = Some(learning);
            break;
        }
    }
    let adopted =
        adopted.context("useful candidate did not graduate within the controlled workload")?;
    let trial_count = adopted.observations.sessions.len();
    let original_trials = serde_json::to_value(&adopted.observations)?;
    let mut monitored = BTreeSet::new();
    let (deployed, enrollment) = episode(&fixture, agent, GOOD).await?;
    ensure!(
        enrollment.assignments.is_empty() && enrollment.monitoring.len() == 1,
        "adopted traffic must have monitoring membership instead of a trial assignment"
    );
    assert_model(&fixture, &deployed, "cheap").await?;
    let deployed_learning = evaluate(&fixture, &deployed).await?;
    monitored.insert(deployed.key()?);
    ensure!(
        serde_json::to_value(&deployed_learning.observations)? == original_trials
            && deployed_learning
                .monitoring_observations
                .sessions
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
                == monitored,
        "deployment feedback changed the trial evidence or monitoring population"
    );
    ensure!(
        deployed_learning.block_status == BlockStatus::Adopted
            && deployed_learning.observations.sessions.len() == trial_count,
        "deployment changed the trial population or lost adoption"
    );
    sessions.push(deployed);

    let mut rolled_back = false;
    let mut bad_sessions = 0;
    for _ in 0..32 {
        let (identity, enrollment) = episode(&fixture, agent, BAD).await?;
        ensure!(
            enrollment.assignments.is_empty() && enrollment.monitoring.len() == 1,
            "quality monitor membership missing"
        );
        assert_model(&fixture, &identity, "cheap").await?;
        let learning = evaluate(&fixture, &identity).await?;
        monitored.insert(identity.key()?);
        sessions.push(identity);
        bad_sessions += 1;
        ensure!(
            serde_json::to_value(&learning.observations)? == original_trials
                && learning
                    .monitoring_observations
                    .sessions
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    == monitored,
            "deployment feedback changed trial evidence or monitoring population"
        );
        if learning.block_status == BlockStatus::RolledBack {
            ensure!(
                learning.monitoring.rollback,
                "withdrawal did not come from the quality monitor"
            );
            ensure!(
                learning.monitoring.severe_sessions == 0
                    && learning.monitoring.recent_quality.observed_families
                        >= crate::evolution::bandit::BanditConfig::default()
                            .recent_guard_minimum_families,
                "quality rollback bypassed its default independent-family requirement"
            );
            rolled_back = true;
            break;
        }
    }
    ensure!(
        rolled_back,
        "recorded low quality failed to withdraw the adopted candidate"
    );
    let (restored, _) = episode(&fixture, agent, GOOD).await?;
    assert_model(&fixture, &restored, "strong").await?;
    evaluate(&fixture, &restored).await?;
    sessions.push(restored);
    let state = service.state().await?;
    ensure!(
        state
            .publications
            .iter()
            .any(|entry| entry.action == "promote")
            && state
                .publications
                .iter()
                .any(|entry| entry.action == "adopted_quality_rollback"),
        "durable promotion/rollback publications missing"
    );
    let before = verify_judge_costs(&fixture, sessions.len()).await?;
    let repeat = EvolutionScheduler::new(fixture.runtime.clone())
        .tick(&fixture.pipeline)
        .await?;
    ensure!(
        repeat.errors.is_empty() && repeat.jobs_completed == 0 && repeat.publications == 0,
        "unchanged evidence repeated evaluation or publication"
    );
    ensure!(
        verify_judge_costs(&fixture, sessions.len()).await? == before,
        "unchanged evidence increased judge spend"
    );
    let mut actual = BTreeSet::new();
    let mut models = BTreeMap::<String, usize>::new();
    for identity in &sessions {
        for execution in fixture.runtime.executions(identity).await? {
            let model = execution
                .settlement
                .as_ref()
                .context("settlement missing")?
                .final_model
                .clone();
            *models.entry(model).or_default() += 1;
            ensure!(
                actual.insert(execution.request_id),
                "request counted in two native sessions"
            );
        }
    }
    let ingress = fixture
        .ingress
        .lock()
        .map_err(|_| anyhow::anyhow!("ingress lock poisoned"))?
        .len();
    ensure!(
        actual.len() == ingress,
        "worker traffic escaped canonical execution accounting"
    );
    eprintln!(
        "{agent}: publication acceptance complete; {trial_count} trial sessions ({baseline} baseline, {challenger} challenger); {bad_sessions} bad monitored sessions before rollback; {} actual model requests {models:?}; judge cost {:?} micro-USD",
        actual.len(),
        before.total_cost_micro_usd
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires the maintained Codex ACP package and worker; controlled native publication workload"]
async fn maintained_codex_publication_and_rollback() -> Result<()> {
    run_publication(
        "codex-acp",
        "BITROUTER_TEST_CODEX_ADAPTER",
        "BITROUTER_TEST_CODEX_WORKER",
    )
    .await
}

#[tokio::test]
#[ignore = "requires the maintained Claude ACP package and worker; controlled native publication workload"]
async fn maintained_claude_publication_and_rollback() -> Result<()> {
    run_publication(
        "claude-acp",
        "BITROUTER_TEST_CLAUDE_ADAPTER",
        "BITROUTER_TEST_CLAUDE_WORKER",
    )
    .await
}
