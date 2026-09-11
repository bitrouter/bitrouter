//! A full terminal campaign; policy and scoring mutations occur through the
//! production terminal or scheduler, never through planted learner/store rows.

use super::*;
use bitrouter::evolution::bandit::Arm;
use bitrouter::evolution::learning::LearningReport;
use std::collections::BTreeSet;

const BLOCK: &str = "native-terminal-trial";

fn trial_values(report: &LearningReport) -> Result<Value> {
    let mut values = serde_json::to_value(&report.observations)?;
    for observation in values["sessions"]
        .as_object_mut()
        .context("trial observations missing")?
        .values_mut()
    {
        // Closing a native capture changes its provenance revision. It must not
        // add an independent sample or change the recorded outcome quantities.
        observation
            .as_object_mut()
            .context("trial observation is not an object")?
            .remove("revision");
    }
    Ok(values)
}

fn fresh_session(
    code: &mut CodeFixture,
    runtime: &tokio::runtime::Runtime,
    service: &NativeService,
    agent: &str,
) -> Result<SessionIdentity> {
    let known = runtime
        .block_on(service.canonical.list("local", agent))?
        .into_iter()
        .map(|session| session.native_session_id)
        .collect::<BTreeSet<_>>();
    code.pty.send(b"\x10")?;
    code.pty.wait_for_text("Commands")?;
    let before = code.pty.checkpoint();
    code.pty.paste("New session")?;
    code.pty.send(b"\r")?;
    code.pty
        .wait_for_screen_inner(Some(&before), "fresh coding session", |screen| {
            screen.contains("activity: ready")
                && !screen.contains("NATIVE_CODE_DONE")
                && !screen.contains("Commands")
        })?;
    runtime.block_on(async {
        tokio::time::timeout(PTY_TIMEOUT, async {
            loop {
                for session in service.canonical.list("local", agent).await? {
                    if !known.contains(&session.native_session_id) {
                        return Ok::<_, anyhow::Error>(SessionIdentity {
                            owner: "local".into(),
                            source: agent.into(),
                            native_session_id: session.native_session_id,
                        });
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("fresh coding session was not captured")?
    })
}

fn send(code: &mut CodeFixture, failed: bool) -> Result<()> {
    let before = code.pty.checkpoint();
    let prompt = if failed {
        format!("{PROMPT} Controlled fixture marker: NATIVE_FAILING_IMPLEMENTATION.")
    } else {
        PROMPT.into()
    };
    code.pty.paste(&prompt)?;
    code.pty.send(b"\r")?;
    finish_coding_turn(code, &before)
}

async fn processed(service: &NativeService, identity: &SessionIdentity) -> Result<LearningReport> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let learning = service.evolution.service("local")?;
            let report = match learning.learning_status(BLOCK).await {
                Err(error) if error.to_string().contains("changed") => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    continue;
                }
                other => other?,
            };
            let key = identity.key()?;
            let observation = report
                .observations
                .sessions
                .get(&key)
                .or_else(|| report.monitoring_observations.sessions.get(&key));
            if observation.is_some_and(|o| {
                o.quality.is_some() && o.total_cost_micro_usd.is_some() && o.latency_ms.is_some()
            }) {
                let state = learning.state().await?;
                let block = state
                    .blocks
                    .get(BLOCK)
                    .context("campaign block disappeared")?;
                // Wait for the existing scheduler to reconcile a closed cohort;
                // no synthetic tick or explicit backend mutation accelerates it.
                if block.status == report.block_status
                    && (block.status != BlockStatus::Exploring || !block.batch.closed)
                    && (!report.monitoring.rollback || block.status == BlockStatus::RolledBack)
                {
                    return Ok(report);
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("coding evidence or background publication did not settle")?
}

async fn verify_checkpoint(
    service: &NativeService,
    identity: &SessionIdentity,
    failed: bool,
    model: &str,
) -> Result<usize> {
    let effective = read_effective(service, identity).await?;
    ensure!(
        !effective.stale && effective.assessment.is_some(),
        "coding assessment missing or stale"
    );
    let checkpoint = effective.checkpoint.context("coding checkpoint missing")?;
    let packet = EvidencePacket::from_checkpoint(
        &service
            .canonical
            .checkpoint_content(identity, &checkpoint.checkpoint_id)
            .await?,
    )?;
    let observations = packet
        .items
        .iter()
        .filter(|item| item.kind == EvidenceKind::ToolObservation)
        .map(|item| item.content.to_string())
        .collect::<Vec<_>>();
    ensure!(
        observations.iter().any(|s| s.contains("Ran 2 tests")),
        "executed unit tests were not recorded"
    );
    ensure!(
        observations
            .iter()
            .any(|s| s.contains("FAILED (failures=2)"))
            == failed,
        "unexpected executed test outcome"
    );
    let resource = effective.resource.context("coding resources missing")?;
    ensure!(
        resource.metering_complete,
        "coding resource accounting is incomplete"
    );
    let executions = service.evolution.executions(identity).await?;
    ensure!(executions.len() >= 2, "tool continuation not metered");
    let mut total = 0_u64;
    let mut matched = false;
    for execution in &executions {
        let settlement = execution
            .settlement
            .as_ref()
            .context("coding request unsettled")?;
        let selected = settlement.final_model == model;
        // Complete candidate policies include their capability fallback. Keep
        // auxiliary baseline work in the candidate session and its full cost.
        let guarded = model == "cheap"
            && settlement.final_model == "strong"
            && execution.route_guard_reason.as_deref()
                == Some("candidate_request_capability_unverified")
            && execution.intent.as_ref().is_some_and(|intent| {
                intent.selected_route == "candidate" && intent.bypass.is_none()
            });
        ensure!(
            selected || guarded,
            "unexpected actual coding route: {execution:?}"
        );
        matched |= selected;
        total += settlement
            .total_cost_micro_usd
            .context("coding request cost unknown")?;
    }
    ensure!(matched, "coding session never executed its selected model");
    ensure!(
        resource
            .requests
            .iter()
            .map(|r| &r.request_id)
            .collect::<BTreeSet<_>>()
            == executions
                .iter()
                .map(|e| &e.request_id)
                .collect::<BTreeSet<_>>(),
        "coding request inventory and checkpoint costs differ"
    );
    ensure!(
        u64::try_from(resource.known_cost_micro_usd)? == total,
        "whole-session coding costs differ"
    );
    Ok(executions.len())
}

fn inspect(code: &mut CodeFixture, state: &str) -> Result<()> {
    open_evolution(code)?;
    choose(code, "Policy block evidence", "Policy blocks")?;
    choose(code, BLOCK, "Original trial recommendation")?;
    code.pty
        .wait_for_text(&format!("Current block state: {state}"))?;
    code.close_to_composer()
}

fn run(agent: &str, adapter: &str, worker: &str) -> Result<()> {
    let started = Instant::now();
    let (runtime, service, mut code) = launch(agent, adapter, worker)?;
    send(&mut code, false)?;
    let initial = runtime.block_on(evaluated(&service, agent))?;
    let mut requests = runtime.block_on(verify_checkpoint(&service, &initial, false, "strong"))?;
    create_candidate(&mut code, agent)?;
    let state = runtime.block_on(service.evolution.service("local")?.state())?;
    let block = state
        .blocks
        .get(BLOCK)
        .context("terminal candidate missing")?;
    ensure!(
        serde_json::to_value(&block.definition.bandit)?
            == serde_json::to_value(BanditConfig::default())?
            && block.definition.batch_sessions == 16,
        "campaign changed production exploration parameters"
    );
    let mut baseline = 0;
    let mut candidate = 0;
    let mut adopted = None;
    for arrival in 1..=512 {
        ensure!(
            started.elapsed() < Duration::from_secs(2400),
            "coding campaign exceeded its bounded deadline at trial {arrival}; baseline {baseline}; candidate {candidate}"
        );
        let identity = fresh_session(&mut code, &runtime, &service, agent)?;
        send(&mut code, false)?;
        let enrollment = runtime
            .block_on(service.evolution.service("local")?.enrollment(&identity))?
            .context("coding enrollment missing")?;
        let assignment = enrollment
            .assignments
            .get(BLOCK)
            .context("fresh coding trial assignment missing")?;
        let model = match assignment.arm {
            Arm::Baseline => {
                baseline += 1;
                "strong"
            }
            Arm::Challenger => {
                candidate += 1;
                "cheap"
            }
        };
        let report = runtime.block_on(processed(&service, &identity))?;
        requests += runtime.block_on(verify_checkpoint(&service, &identity, false, model))?;
        ensure!(
            report.observations.sessions.len() == arrival,
            "coding trial samples were lost or duplicated"
        );
        ensure!(
            report.block_status != BlockStatus::RolledBack,
            "useful coding fixture rolled back: {}",
            report.plan.reason
        );
        if arrival % 16 == 0 || report.block_status == BlockStatus::Adopted {
            eprintln!(
                "{agent}: coding trial {arrival}; baseline {baseline}; candidate {candidate}; {:?}; posterior lower {} ppm",
                report.block_status, report.plan.monte_carlo_lower_ppm
            );
        }
        if report.block_status == BlockStatus::Adopted {
            adopted = Some(report);
            break;
        }
    }
    let adopted =
        adopted.context("coding candidate did not adopt within the controlled workload")?;
    ensure!(
        baseline >= 20 && candidate >= 20,
        "coding adoption bypassed evidence minimum"
    );
    inspect(&mut code, "Adopted")?;
    let trials = trial_values(&adopted)?;
    let mut withdrawn = None;
    for monitor in 1..=12 {
        let failed = monitor > 1;
        let identity = fresh_session(&mut code, &runtime, &service, agent)?;
        send(&mut code, failed)?;
        let enrollment = runtime
            .block_on(service.evolution.service("local")?.enrollment(&identity))?
            .context("coding monitoring membership missing")?;
        ensure!(
            enrollment.assignments.is_empty() && enrollment.monitoring.contains_key(BLOCK),
            "adopted coding traffic was mixed into the randomized trial"
        );
        let report = runtime.block_on(processed(&service, &identity))?;
        requests += runtime.block_on(verify_checkpoint(&service, &identity, failed, "cheap"))?;
        ensure!(
            trial_values(&report)? == trials,
            "coding monitoring changed original trial outcome values: before {trials}; after {}",
            trial_values(&report)?
        );
        if report.block_status == BlockStatus::RolledBack {
            ensure!(
                monitor >= 8 && report.monitoring.rollback,
                "coding rollback bypassed recent-family guard: monitor {monitor}; monitoring rollback {}; trial recommendation {:?}; lower {} ppm; seed {}; actions {:?}",
                report.monitoring.rollback,
                report.plan.recommendation,
                report.plan.monte_carlo_lower_ppm,
                report.plan.seed,
                report
                    .publications
                    .iter()
                    .map(|publication| &publication.action)
                    .collect::<Vec<_>>()
            );
            withdrawn = Some((identity, monitor, report));
            break;
        }
    }
    let (identity, monitor_sessions, report) =
        withdrawn.context("recorded failing coding tests did not trigger rollback")?;
    inspect(&mut code, "RolledBack")?;
    ensure!(
        report.publications.iter().any(|p| p.action == "promote")
            && report
                .publications
                .iter()
                .any(|p| p.action == "adopted_quality_rollback"),
        "automatic publication history missing"
    );
    ensure!(
        report
            .publications
            .iter()
            .all(|p| p.action != "operator_restore"),
        "coding campaign used operator withdrawal"
    );
    assert_restored_dispatch(&mut code, &runtime, &service, &identity)?;
    open_evolution(&mut code)?;
    choose(&mut code, "Evolution mode", " Evolution mode ─")?;
    choose(&mut code, "Off", "Checkpoint evaluation and evolution")?;
    ensure!(
        runtime
            .block_on(service.evolution.service("local")?.state())?
            .mode
            == EvolutionMode::Off,
        "terminal did not disable evolution"
    );
    code.close_to_composer()?;
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()?;
    println!(
        "{}",
        json!({"agent":agent,"trial_sessions":baseline+candidate,"baseline_trials":baseline,"candidate_trials":candidate,"monitor_sessions":monitor_sessions,"recorded_trial_and_monitor_requests_with_initial":requests,"real_code_and_tests":true,"automatic_adoption":true,"automatic_quality_rollback":true,"baseline_restored_while_automatic":true,"terminal_restored":true,"default_exploration_parameters":true,"scope":"actual native tool execution and terminal serving acceptance; controlled model replies, prices, latency and rubric labels, not historical calibration or live routing benefit"})
    );
    Ok(())
}

#[test]
#[ignore = "requires maintained Codex ACP and a bounded multi-session coding campaign"]
fn maintained_codex_tui_coding_publication_and_rollback() -> Result<()> {
    run(
        "codex-acp",
        "BITROUTER_TEST_CODEX_ADAPTER",
        "BITROUTER_TEST_CODEX_WORKER",
    )
}

#[test]
#[ignore = "requires maintained Claude ACP and a bounded multi-session coding campaign"]
fn maintained_claude_tui_coding_publication_and_rollback() -> Result<()> {
    run(
        "claude-acp",
        "BITROUTER_TEST_CLAUDE_ADAPTER",
        "BITROUTER_TEST_CLAUDE_WORKER",
    )
}
