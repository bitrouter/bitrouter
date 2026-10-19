//! Actual ACP start with an intentionally missing Goal observation commit.
use super::*;
use bitrouter_orchestrator::goal::completion::{Condition, Specification};
use bitrouter_orchestrator::goal::workflow::TaskSpec;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_failure_retains_actual_phase_and_reaped_process_receipt() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let home = temporary.path();
    for name in ["controls", "captures", "work"] {
        tokio::fs::create_dir_all(home.join(name)).await?;
    }
    let configuration = format!(
        "inherit_defaults: false\nregistry: {{enabled: false}}\nserver: {{listen: '127.0.0.1:0', skip_auth: true}}\nagents:\n  goal-peer:\n    name: goal-peer\n    transport:\n      type: stdio\n      command: {}\n      env:\n        BRO_GOAL_FIXTURE_CONTROL: {}\n        BRO_GOAL_FIXTURE_CAPTURE: {}\n        BRO_GOAL_FIXTURE_FAIL_INITIALIZE: '1'\n",
        serde_json::to_string(&peer_executable()?)?,
        serde_json::to_string(&home.join("controls"))?,
        serde_json::to_string(&home.join("captures"))?,
    );
    let config_path = home.join("bitrouter.yaml");
    tokio::fs::write(&config_path, &configuration).await?;
    let config = bitrouter_sdk::config::parse_with(&configuration, |_| None)?;
    let supervisor = Supervisor::open(
        crate::paths::ConfigSource::File(config_path),
        Arc::new(ConfigRoutingTable::from_config(config)),
    )
    .await?;
    let result = async {
        let SessionResponse::Authorized { grant } = supervisor
            .dispatch(SessionRequest::authorize(
                "startup-receipt-owner".into(),
                BTreeSet::from([
                    SessionScope::Start,
                    SessionScope::Peek,
                    SessionScope::Transcript,
                ]),
            ))
            .await?
        else {
            anyhow::bail!("startup receipt grant missing")
        };
        let started = supervisor
            .dispatch(SessionRequest::authorized(
                &grant,
                SessionCommand::Start(Box::new(StartRunRequest {
                    action_request_id: "original-failed-start".into(),
                    client_id: Some(grant.client_id.clone()),
                    label: None,
                    agent_id: "goal-peer".into(),
                    prompt: Some("must never be admitted".into()),
                    cwd: home.join("work"),
                    routing: Default::default(),
                    launch: Default::default(),
                    session: Default::default(),
                    presentation: crate::supervisor::Presentation::Background,
                    parent_run_id: None,
                    allow_shared_directory: false,
                    permission_policy: Default::default(),
                    result_schema: None,
                })),
            ))
            .await;
        assert!(started.is_err());
        let snapshot = supervisor
            .start_snapshot(&grant, "original-failed-start")
            .await?
            .context("original failed startup mapping missing")?;
        anyhow::ensure!(
            snapshot.confirmed_startup_abort(),
            "actual startup receipt absent: process {:?}, phase {}, native {:?}, failure {:?}",
            snapshot.process,
            snapshot.startup_aborted_before_session,
            snapshot.native_session_id,
            snapshot.failure
        );
        let SessionResponse::Events { replay } = supervisor
            .dispatch(SessionRequest::authorized(
                &grant,
                SessionCommand::Events {
                    run_id: snapshot.run_id.clone(),
                    after_seq: 0,
                },
            ))
            .await?
        else {
            anyhow::bail!("original failed startup history missing")
        };
        assert!(replay.history_complete);
        assert!(!replay.events.iter().any(|event| matches!(
            event.kind,
            crate::supervisor::SessionEventKind::UserPrompt { .. }
                | crate::supervisor::SessionEventKind::NativeSessionSelected { .. }
        )));
        Ok(())
    }
    .await;
    supervisor.shutdown().await;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_goal_recovers_original_start_and_stops_without_relaunch() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let home = temporary.path();
    let captures = home.join("captures");
    let controls = home.join("controls");
    for path in [&captures, &controls] {
        tokio::fs::create_dir_all(path).await?;
    }
    let config_path = home.join("bitrouter.yaml");
    let configuration = format!(
        "inherit_defaults: false\nregistry: {{enabled: false}}\nserver: {{listen: '127.0.0.1:0', skip_auth: true}}\ndatabase: {{url: 'sqlite://{}?mode=rwc'}}\nagents:\n  goal-peer:\n    name: goal-peer\n    transport:\n      type: stdio\n      command: {}\n      env:\n        BRO_GOAL_FIXTURE_CONTROL: {}\n        BRO_GOAL_FIXTURE_CAPTURE: {}\n",
        home.join("goals.db").display(),
        serde_json::to_string(&peer_executable()?)?,
        serde_json::to_string(&controls)?,
        serde_json::to_string(&captures)?
    );
    tokio::fs::write(&config_path, &configuration).await?;
    let config = bitrouter_sdk::config::parse_with(&configuration, |_| None)?;
    let assembled = crate::assemble::build_app_with_path(&config, Some(&config_path)).await?;
    let goals = Arc::new(GoalService::new(Arc::new(
        crate::goal_store::DatabaseGoalStore::new(assembled.db),
    )));
    let routing = Arc::new(ConfigRoutingTable::from_config(config));
    let supervisor = Supervisor::open(
        crate::paths::ConfigSource::File(config_path),
        routing.clone(),
    )
    .await?;
    let SessionResponse::Authorized { grant } = supervisor
        .dispatch(SessionRequest::authorize(
            "lost-observation-owner".into(),
            BTreeSet::from([
                SessionScope::Start,
                SessionScope::Peek,
                SessionScope::Transcript,
                SessionScope::Respond,
                SessionScope::Stop,
            ]),
        ))
        .await?
    else {
        anyhow::bail!("original owner grant missing")
    };
    let coordinator = Coordinator {
        id: "lost-observation-owner".into(),
        app: Arc::new(assembled.app),
        goals: goals.clone(),
        supervisor: supervisor.clone(),
        routing,
        workspaces: home.join("goal-workspaces"),
        home: home.into(),
        grant,
        workspace_gate: tokio::sync::Mutex::new(()),
        stop_gate: tokio::sync::Mutex::new(()),
        gate: crate::daemon_handoff::HandoffGate::default(),
    };
    let result = async {
        for unknown in [false, true] {
            let execution_id = format!("original-dispatch-{unknown}");
            let workspace = home.join(&execution_id);
            tokio::fs::create_dir_all(&workspace).await?;
            let GoalReply::Accepted { operation } = goals.execute("local", GoalCommand::Create {
                operation_id: format!("create-{unknown}"), title: "Original dispatch recovery".into(),
                objective: "Preserve the original execution without replay".into(), workspace: workspace.clone(),
                budget_limits: Default::default(),
            }, now()?).await? else { anyhow::bail!("creation receipt missing") };
            let id = operation.goal_id;
            let current = read_goal(&goals, &id).await?;
            coordinator.claim_owner(&current).await?;
            for (step, change) in [
                Change::RequestPlan { attempt_id: "fixture-plan".into() },
                Change::InstallPlan { attempt_id: "fixture-plan".into(), plan: Plan {
                    tasks: vec![TaskSpec { id: "left".into(), title: "Left".into(), instruction: "Keep original evidence".into(), harness: "goal-peer".into(), dependencies: vec![], checks: vec![], resources: Default::default() }],
                    completion: Specification { conditions: vec![Condition { id: "original".into(), description: "Verified original result".into() }], checks: vec![], human_acceptance_required: false },
                }, available_harnesses: BTreeSet::from(["goal-peer".into()]) },
                Change::RequestExecution { task_id: "left".into(), execution_id: execution_id.clone(), workspace: workspace.clone() },
                Change::RegisterWorkspace { execution_id: execution_id.clone(), base_commit: "a".repeat(40) },
                Change::RequestDispatch { execution_id: execution_id.clone() },
            ].into_iter().enumerate() {
                coordinator.change(&current, change).await.with_context(|| format!("prestart admission step {step}"))?;
            }
            let SessionResponse::Started { snapshot, .. } = supervisor.dispatch(SessionRequest::authorized(
                &coordinator.grant, SessionCommand::Start(Box::new(StartRunRequest {
                    action_request_id: execution_id.clone(), client_id: Some(coordinator.grant.client_id.clone()),
                    label: None, agent_id: "goal-peer".into(), prompt: None, cwd: workspace,
                    routing: Default::default(), launch: Default::default(), session: Default::default(),
                    presentation: crate::supervisor::Presentation::Background, parent_run_id: None,
                    allow_shared_directory: false, permission_policy: Default::default(), result_schema: None,
                })))).await? else { anyhow::bail!("actual ACP start missing") };
            assert_eq!(snapshot.process, ProcessState::Running);
            // Crash/failed transaction window: no Running observation reaches the Goal.
            if unknown {
                coordinator.change(&current, Change::ObserveExecution {
                    execution_id: execution_id.clone(), observation: Observation::Unknown {
                        run_id: None, reason: "Original observation commit failed".into(),
                    },
                }).await?;
            }
            let current = read_goal(&goals, &id).await?;
            goals.execute("local", GoalCommand::Control { operation_id: format!("pause-{unknown}"),
                goal_id: id.clone(), expected_revision: current.revision, intent: RunIntent::Pause,
            }, now()?).await?;
            let paused = read_goal(&goals, &id).await?;
            coordinator.drive_task(&paused, &paused.workflow.tasks[0]).await.context("recover original start mapping")?;
            let recovered = read_goal(&goals, &id).await?;
            assert_eq!(recovered.run_intent, RunIntent::Pause);
            assert_eq!(recovered.workflow.tasks[0].executions.len(), 1);
            assert!(matches!(&recovered.workflow.tasks[0].executions[0].observation,
                Observation::Running { run_id, native_session_id } if run_id == &snapshot.run_id && native_session_id == &snapshot.native_session_id));
            coordinator.drive_controls(&recovered).await.context("stop original recovered process")?;
            let stopped = read_goal(&goals, &id).await?;
            let execution = &stopped.workflow.tasks[0].executions[0];
            assert!(matches!(execution.stop, Some(StopState::Confirmed { .. })));
            assert!(!execution.effects_reconciled());
            assert_eq!(stopped.run_intent, RunIntent::Pause);
            assert!(!stopped.workflow.completion.completed_for(1));
            assert_eq!(stopped.workflow.tasks[0].executions.len(), 1);
            let SessionResponse::Snapshot { snapshot: original } = supervisor.dispatch(SessionRequest::authorized(
                &coordinator.grant, SessionCommand::Snapshot { run_id: snapshot.run_id.clone() }
            )).await? else { anyhow::bail!("original stopped process missing") };
            assert_eq!(original.process, ProcessState::Stopped);
        }
        let mut files = tokio::fs::read_dir(&captures).await?;
        let mut starts = 0;
        while let Some(file) = files.next_entry().await? {
            for line in tokio::fs::read_to_string(file.path()).await?.lines() {
                let message: serde_json::Value = serde_json::from_str(line)?;
                starts += usize::from(message["method"] == "session/new");
                assert_ne!(message["method"], "session/prompt");
            }
        }
        assert_eq!(starts, 2, "recovery must not dispatch another ACP session");
        Ok(())
    }.await;
    supervisor.shutdown().await;
    result
}

async fn read_goal(goals: &GoalService, id: &str) -> Result<GoalSnapshot> {
    let GoalReply::Snapshot { goal } = goals
        .execute("local", GoalCommand::Get { goal_id: id.into() }, now()?)
        .await?
    else {
        anyhow::bail!("original Goal snapshot missing")
    };
    Ok(*goal)
}
