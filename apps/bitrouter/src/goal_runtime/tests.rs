//! Full coordinator transport tests. Provider judgments are explicit fixtures.
use super::*;
use crate::daemon::{
    AcpControlPlane, DaemonCommand, DaemonResponse, LocalControlOptions, NoopObserveStatus,
    NoopReloader, bind_control_socket, run_bound_control_socket, send_command,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

#[path = "../../../../crates/bitrouter-orchestrator/tests/support/example_binary.rs"]
mod example_binary;

pub(super) fn peer_executable() -> Result<PathBuf> {
    static BINARY: std::sync::OnceLock<std::result::Result<PathBuf, String>> =
        std::sync::OnceLock::new();
    BINARY
        .get_or_init(|| example_binary::build("bitrouter", "goal_acp_fixture"))
        .clone()
        .map_err(anyhow::Error::msg)
}

#[path = "../../tests/support/goal_provider.rs"]
mod provider;

async fn rpc(socket: &Path, command: GoalCommand) -> Result<GoalReply> {
    match send_command(
        socket,
        &DaemonCommand::Goals {
            protocol: bitrouter_orchestrator::goal::PROTOCOL_VERSION,
            command,
        },
    )
    .await?
    {
        DaemonResponse::Goals { response, .. } => Ok(response?),
        other => anyhow::bail!("unexpected fixture Goal RPC: {other:?}"),
    }
}
async fn head(socket: &Path, id: &str) -> Result<GoalSnapshot> {
    match rpc(socket, GoalCommand::Get { goal_id: id.into() }).await? {
        GoalReply::Snapshot { goal } => Ok(*goal),
        other => anyhow::bail!("unexpected fixture snapshot: {other:?}"),
    }
}
async fn wait_for(
    socket: &Path,
    id: &str,
    condition: impl Fn(&GoalSnapshot) -> bool,
) -> Result<GoalSnapshot> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let goal = head(socket, id).await?;
        if condition(&goal) {
            return Ok(goal);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "fixture Goal did not reach expected state: {:?}",
            goal.workflow
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_goal_runs_parallel_acp_handoff_input_and_verified_delivery() -> Result<()> {
    parallel_fixture(Scenario::Complete).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn goal_selected_resources_are_persisted_and_sent_as_actual_acp_material() -> Result<()> {
    parallel_fixture(Scenario::Resources).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn goal_model_route_is_confirmed_before_prompt_and_persisted_through_delivery() -> Result<()>
{
    parallel_fixture(Scenario::ModelRoute).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paused_goal_edit_stays_paused_and_resumes_with_current_branch_versions() -> Result<()> {
    parallel_fixture(Scenario::PauseEdit).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopped_process_without_terminal_tool_evidence_cannot_restart_or_complete() -> Result<()> {
    parallel_fixture(Scenario::UnresolvedTool).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_branch_edit_preserves_independent_run_and_renews_pending_permission() -> Result<()>
{
    parallel_fixture(Scenario::ActiveEdit).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Complete,
    Resources,
    ModelRoute,
    PauseEdit,
    UnresolvedTool,
    ActiveEdit,
}

async fn control(socket: &Path, id: &str, operation: &str, intent: RunIntent) -> Result<()> {
    for _ in 0..8 {
        let current = head(socket, id).await?;
        match rpc(
            socket,
            GoalCommand::Control {
                operation_id: operation.into(),
                goal_id: id.into(),
                expected_revision: current.revision,
                intent,
            },
        )
        .await
        {
            Ok(GoalReply::Accepted { .. }) => return Ok(()),
            Err(error)
                if error.downcast_ref::<bitrouter_orchestrator::goal::GoalError>()
                    == Some(&bitrouter_orchestrator::goal::GoalError::Conflict) => {}
            other => anyhow::bail!("explicit fixture control was not admitted: {other:?}"),
        }
    }
    anyhow::bail!("fixture control remained in conflict")
}

async fn parallel_fixture(scenario: Scenario) -> Result<()> {
    let pause_edit = !matches!(
        scenario,
        Scenario::Complete | Scenario::Resources | Scenario::ModelRoute
    );
    let active_edit = scenario == Scenario::ActiveEdit;
    let binary = peer_executable()?;
    let temporary = tempfile::tempdir()?;
    let home = temporary.path();
    let workspace = home.join("original");
    let controls = home.join("controls");
    let captures = home.join("captures");
    for path in [&workspace, &controls, &captures] {
        tokio::fs::create_dir_all(path).await?;
    }
    tokio::fs::write(workspace.join("seed.txt"), "original input\n").await?;
    if scenario == Scenario::Resources {
        let skill_dir = workspace.join("skills/goal-resource-fixture");
        tokio::fs::create_dir_all(&skill_dir).await?;
        tokio::fs::write(skill_dir.join("SKILL.md"), "---\nname: goal-resource-fixture\ndescription: Fixture material for selected context\n---\nGOAL_RESOURCE_SKILL_MARKER: report actual verification.\n").await?;
    }
    git(&workspace, &["init".as_ref(), "-q".as_ref()]).await?;
    git(&workspace, &["add".as_ref(), "seed.txt".as_ref()]).await?;
    git(
        &workspace,
        &[
            "-c".as_ref(),
            "user.name=Goal Fixture".as_ref(),
            "-c".as_ref(),
            "user.email=goal-fixture@example.test".as_ref(),
            "-c".as_ref(),
            "core.hooksPath=/dev/null".as_ref(),
            "commit".as_ref(),
            "--no-gpg-sign".as_ref(),
            "-qm".as_ref(),
            "fixture baseline".as_ref(),
        ],
    )
    .await?;
    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(provider::model)
        .mount(&backend)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(provider::typed)
        .mount(&backend)
        .await;
    let config_path = home.join("bitrouter.yaml");
    let config_text = format!(
        "inherit_defaults: false\nregistry: {{enabled: false}}\nserver: {{listen: '127.0.0.1:0', skip_auth: true}}\ndatabase: {{url: 'sqlite::memory:'}}\nchat: {{model: goal-model}}\nproviders:\n  fixture:\n    api_base: {}/v1\n    api_key: goal-fixture-key\n    api_protocol: [{{'*': chat_completions}}]\n    models: [{{id: test-model}}]\nmodels:\n  goal-model:\n    endpoints: [{{provider: fixture, service_id: test-model}}]\ndecision_model:\n  model: fixture-input\n  base_url: {}\n  api_key_env: USER\nagents:\n  goal-peer:\n    name: goal-peer\n    transport:\n      type: stdio\n      command: {}\n      args: []\n      env:\n        BRO_GOAL_FIXTURE_CONTROL: {}\n        BRO_GOAL_FIXTURE_CAPTURE: {}\n",
        backend.uri(),
        backend.uri(),
        serde_json::to_string(&binary)?,
        serde_json::to_string(&controls)?,
        serde_json::to_string(&captures)?
    );
    let config_text = if active_edit {
        format!("{config_text}        BRO_GOAL_FIXTURE_RIGHT_MANUAL: '1'\n")
    } else {
        config_text
    };
    let config_text = if scenario == Scenario::ModelRoute {
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&backend)
            .await;
        let listen = backend.uri().trim_start_matches("http://").to_owned();
        config_text.replace("127.0.0.1:0", &listen).replace(
            "args: []",
            "args: ['@agentclientprotocol/codex-acp@1.10.0']",
        )
    } else {
        config_text
    };
    let config_text = if scenario == Scenario::UnresolvedTool {
        format!("{config_text}        BRO_GOAL_FIXTURE_OMIT_TOOL_COMPLETION: '1'\n")
    } else {
        config_text
    };
    let config_text = if scenario == Scenario::Resources {
        let mcp = example_binary::build("bitrouter-orchestrator", "harness_mcp_fixture")
            .map_err(anyhow::Error::msg)?;
        format!(
            "{config_text}mcp_servers:\n  selected-resource-server:\n    name: selected-resource-server\n    transport:\n      type: stdio\n      command: {}\n      env: {{RESOURCE_TOKEN: 'private-mcp-fixture-token'}}\n  unselected-resource-server:\n    name: unselected-resource-server\n    transport:\n      type: stdio\n      command: {}\n",
            serde_json::to_string(&mcp)?,
            serde_json::to_string(&mcp)?
        )
    } else {
        config_text
    };
    tokio::fs::write(&config_path, &config_text).await?;
    let config = bitrouter_sdk::config::parse_with(&config_text, |_| None)?;
    let assembled = crate::assemble::build_app_with_path(&config, Some(&config_path)).await?;
    let app = Arc::new(assembled.app);
    let goals = Arc::new(GoalService::new(Arc::new(
        crate::goal_store::DatabaseGoalStore::new(assembled.db.clone()),
    )));
    let source = crate::paths::ConfigSource::File(config_path);
    let socket = if scenario == Scenario::ModelRoute {
        crate::daemon::socket_path_for(&source, &config)
    } else {
        home.join("goal.sock")
    };
    let routing = Arc::new(ConfigRoutingTable::from_config(config));
    let supervisor = Supervisor::open(source, routing.clone()).await?;
    let gate = crate::daemon_handoff::HandoffGate::default();
    let runtime = GoalRuntime::start(
        app.clone(),
        goals.clone(),
        supervisor.clone(),
        routing,
        home,
        gate.clone(),
    )
    .await?;
    let bound = bind_control_socket(&socket).await?;
    let ipc = tokio::spawn(run_bound_control_socket(
        bound,
        app,
        "127.0.0.1:0".into(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
        AcpControlPlane {
            runtime: assembled.acp_runtime,
            metering: crate::metering::MeteringStore::new(assembled.db),
            inventory: None,
            evolution: None,
        },
        LocalControlOptions {
            administration: None,
            goals: Some(goals),
            gate,
            cli_owned: false,
        },
    ));
    let result = async {
        let GoalReply::Directory { page } = rpc(&socket,GoalCommand::List {
            after:0,cutoff:None,limit:16,
        }).await? else { anyhow::bail!("cold directory read omitted its page"); };
        assert!(page.goals.is_empty());
        assert!(backend.received_requests().await.context("fixture request capture unavailable")?.is_empty());
        assert!(tokio::fs::read_dir(&captures).await?.next_entry().await?.is_none());
        let GoalReply::Accepted { operation } = rpc(&socket,GoalCommand::Create {
            operation_id:"wire-goal".into(),title:"Fixture parallel delivery".into(),workspace:workspace.clone(),
            objective: if scenario == Scenario::Resources {
                "RESOURCE_SELECTION_FIXTURE: Deliver LEFT and RIGHT files in parallel, then join their actual contents. Configure the applicable MCP server and provide the goal resource skill and seed context to LEFT. BRO must verify all three files. No human acceptance is required.".into()
            } else if scenario == Scenario::ModelRoute {
                "MODEL_ROUTE_SELECTION_FIXTURE: Deliver LEFT and RIGHT files in parallel, then join their actual contents. Select the available BRO route for every task. BRO must verify all three files. No human acceptance is required.".into()
            } else { "Deliver LEFT and RIGHT files in parallel, then join their actual contents. BRO must verify all three files. No human acceptance is required.".into() },
            budget_limits:Default::default(),
        }).await? else { anyhow::bail!("creation omitted its receipt"); };
        let id = operation.goal_id;
        let running = wait_for(&socket,&id,|goal| goal.workflow.tasks.iter().filter(|task|
            task.executions.last().is_some_and(|execution| matches!(execution.observation,Observation::Running { .. }))).count() == 2
                && controls.join("started-left").exists() && controls.join("started-right").exists()).await?;
        assert_eq!(running.requirements_revision,1);
        assert!(controls.join("started-left").exists());
        assert!(controls.join("started-right").exists());
        assert!(!controls.join("started-join").exists());
        let mut accepted = false;
        for _ in 0..8 {
            let current = head(&socket,&id).await?;
            match rpc(&socket,GoalCommand::Input { operation_id:"question-during-work".into(),goal_id:id.clone(),
                expected_revision:current.revision,text:"What is currently active? Do not change anything.".into(),task_reference:None }).await {
                Ok(GoalReply::Accepted { .. }) => { accepted = true; break; },
                Err(error) if error.downcast_ref::<bitrouter_orchestrator::goal::GoalError>()
                    == Some(&bitrouter_orchestrator::goal::GoalError::Conflict) => {},
                other => anyhow::bail!("fixture input was not admitted: {other:?}"),
            }
        }
        assert!(accepted);
        let answered = wait_for(&socket,&id,|goal| goal.inputs.latest.as_ref().is_some_and(|input|
            input.input_id == "question-during-work")).await?;
        let input = answered.inputs.latest.context("missing input result")?;
        assert!(input.answer.is_some());
        assert!(!input.requirements_saved);
        assert!(input.control_saved.is_none());
        assert!(input.classification.is_some());
        assert_eq!(answered.requirements_revision,1);
        assert_eq!(answered.workflow.tasks.iter().filter(|task|
            task.executions.last().is_some_and(|execution| matches!(execution.observation,Observation::Running { .. }))).count(),2);
        assert!(!answered.workflow.completion.completed_for(1));
        let expected_version = if pause_edit {
            let paused = if active_edit {
                wait_for(&socket,&id,|goal| controls.join("completed-left").exists() && goal.workflow.decisions.iter().any(|decision|
                    decision.run_id == goal.workflow.tasks.iter().find(|task|task.spec.id == "right").and_then(|task|task.executions.last()).and_then(|execution|
                        match &execution.observation { Observation::Running {run_id,..} => Some(run_id.clone()), _ => None }).unwrap_or_default()
                    && decision.state == bitrouter_orchestrator::goal::decision::PermissionState::Pending)).await?
            } else {
                wait_for(&socket,&id,|_|controls.join("completed-left").exists() && controls.join("completed-right").exists()).await?;
                control(&socket,&id,"explicit-pause",RunIntent::Pause).await?;
                wait_for(&socket,&id,|goal| goal.run_intent == RunIntent::Pause
                    && goal.workflow.tasks.iter().filter(|task| !task.executions.is_empty()).all(|task|
                        task.executions.last().is_some_and(|execution|matches!(execution.stop,Some(StopState::Confirmed {..}))))).await?
            };
            let original_right = paused.workflow.tasks.iter().find(|task|task.spec.id == "right").context("right task missing")?.executions.last().context("right execution missing")?.id.clone();
            let old_right_permission = paused.workflow.decisions.iter().find(|decision| decision.execution_id == original_right).map(|decision|decision.id.clone());
            let old_left = paused.workflow.tasks.iter().find(|task|task.spec.id == "left").context("left task missing")?.executions.last().context("left execution missing")?.id.clone();
            let mut accepted = false;
            for _ in 0..8 {
                let current = head(&socket,&id).await?;
                match rpc(&socket,GoalCommand::Input { operation_id:"edit-while-paused".into(),goal_id:id.clone(),
                    expected_revision:current.revision,text:"FIXTURE_NEW_LEFT_RULE: keep LEFT newline and use these updated instructions when left executes again; do not resume.".into(),task_reference:Some("left".into()) }).await {
                    Ok(GoalReply::Accepted { .. }) => { accepted = true; break; },
                    Err(error) if error.downcast_ref::<bitrouter_orchestrator::goal::GoalError>() == Some(&bitrouter_orchestrator::goal::GoalError::Conflict) => {},
                    other => anyhow::bail!("paused fixture edit was not admitted: {other:?}"),
                }
            }
            assert!(accepted);
            let edited = wait_for(&socket,&id,|goal|goal.inputs.latest.as_ref().is_some_and(|input|input.input_id == "edit-while-paused")).await?;
            assert_eq!(edited.run_intent,if active_edit {RunIntent::Run} else {RunIntent::Pause});
            assert_eq!(edited.requirements_revision,2);
            let edit = edited.inputs.latest.as_ref().context("edit result missing")?;
            assert!(edit.requirements_saved);
            assert_eq!(edit.affected_tasks,vec!["join","left"]);
            assert!(edit.control_saved.is_none());
            for task in &edited.workflow.tasks {
                assert_eq!(task.requirements_revision,if task.spec.id == "right" {1} else {2});
            }
            assert!(!edited.workflow.completion.completed_for(2));
            assert!(!controls.join("started-join").exists());
            if !active_edit {
                assert_eq!(edited.workflow.tasks.iter().map(|task|task.executions.len()).sum::<usize>(),2);
                control(&socket,&id,"explicit-resume",RunIntent::Run).await?;
            } else {
                let old = old_right_permission.context("original pending permission missing")?;
                assert!(rpc(&socket,GoalCommand::RespondDecision { operation_id:"stale-right-response".into(),goal_id:id.clone(),decision_id:old.clone(),
                    expected_requirements_revision:2,option_id:"allow-once".into() }).await.is_err());
                let renewed = wait_for(&socket,&id,|goal| goal.workflow.decisions.iter().any(|decision|
                    decision.execution_id == original_right && decision.id != old && decision.requirements_revision == 2
                    && decision.state == bitrouter_orchestrator::goal::decision::PermissionState::Pending)).await?;
                let decision = renewed.workflow.decisions.iter().find(|decision| decision.execution_id == original_right
                    && decision.requirements_revision == 2 && decision.state == bitrouter_orchestrator::goal::decision::PermissionState::Pending).context("renewed permission missing")?;
                let right = renewed.workflow.tasks.iter().find(|task|task.spec.id == "right").context("right task missing")?;
                assert_eq!(right.executions.len(),1);
                assert_eq!(right.executions[0].id,original_right);
                assert!(right.executions[0].stop.is_none());
                assert_eq!(right.requirements_revision,1);
                rpc(&socket,GoalCommand::RespondDecision { operation_id:"current-right-response".into(),goal_id:id.clone(),decision_id:decision.id.clone(),
                    expected_requirements_revision:2,option_id:"allow-once".into() }).await?;
            }
            if scenario == Scenario::UnresolvedTool {
                let blocked = wait_for(&socket,&id,|goal|goal.workflow.tasks.iter().filter(|task|!task.executions.is_empty()).all(|task|
                    task.executions.last().is_some_and(|execution|matches!(execution.reconciliation,
                        Some(bitrouter_orchestrator::goal::workflow::LocalReconciliation::Blocked {..}))))).await?;
                assert_eq!(blocked.run_intent,RunIntent::Run);
                assert_eq!(blocked.workflow.tasks.iter().map(|task|task.executions.len()).sum::<usize>(),2);
                assert!(!blocked.workflow.completion.completed_for(2));
                assert!(!controls.join("started-join").exists());
                assert!(blocked.workflow.tasks.iter().flat_map(|task|&task.executions).any(|execution|
                    matches!(&execution.reconciliation,Some(bitrouter_orchestrator::goal::workflow::LocalReconciliation::Blocked {reason,..})
                        if reason.contains("still unresolved"))));
                return Ok(());
            }
            let resumed = wait_for(&socket,&id,|goal|goal.workflow.tasks.iter().find(|task|task.spec.id == "left")
                .and_then(|task|task.executions.last()).is_some_and(|execution|execution.id != old_left && execution.task_revision == 2
                    && matches!(execution.observation,Observation::Running {..}))).await?;
            assert_eq!(resumed.run_intent,RunIntent::Run);
            let mut adopted = false;
            let mut files = tokio::fs::read_dir(&captures).await?;
            while let Some(file) = files.next_entry().await? {
                adopted |= tokio::fs::read_to_string(file.path()).await?.contains("FIXTURE_NEW_LEFT_RULE");
            }
            assert!(adopted,"replacement harness never received the committed branch instructions");
            2
        } else { 1 };
        tokio::fs::write(controls.join("release"),"release fixture only").await?;
        let completed = wait_for(&socket,&id,|goal| goal.workflow.completion.completed_for(expected_version)).await?;
        let delivery = completed.workflow.completion.last_completed.as_ref().context("missing verified delivery")?;
        assert_eq!(delivery.requirements_revision,expected_version);
        assert!(!workspace.join("left.txt").exists());
        assert!(!workspace.join("right.txt").exists());
        assert!(controls.join("started-join").exists());
        let root = crate::goal_artifacts::root(&home.join("goal-workspaces"),&id);
        let material = crate::goal_artifacts::material(&root,&delivery.delivery).await?;
        let material = serde_json::to_string(&material)?;
        assert!(material.contains("left.txt") && material.contains("right.txt") && material.contains("joined.txt"));
        assert!(completed.workflow.tasks.iter().all(|task| task.executions.last().is_some_and(|execution|
            execution.handoff.is_some() && matches!(execution.stop,Some(StopState::Confirmed { .. })))));
        let requests = backend.received_requests().await.context("fixture request capture unavailable")?;
        assert!(requests.iter().any(|request| request.url.path() == "/v1/systemone"));
        assert!(requests.iter().filter(|request| request.url.path() == "/v1/chat/completions").count() >= 6);
        if scenario == Scenario::ModelRoute {
            for task in &completed.workflow.tasks {
                assert_eq!(task.spec.resources.model_route.as_deref(), Some("goal-model"));
                let execution = task.executions.last().context("missing model route execution")?;
                assert_eq!(execution.confirmed_model_route.as_deref(), Some("goal-model"));
                let Observation::Candidate { run_id, .. } = &execution.observation else {
                    anyhow::bail!("confirmed model execution did not produce candidate");
                };
                let SessionResponse::Authorized { grant } = supervisor.dispatch(SessionRequest::authorize(
                    "model-route-test".into(), BTreeSet::from([crate::supervisor::SessionScope::Peek]))).await? else {
                    anyhow::bail!("route inspection grant missing");
                };
                let response = supervisor.dispatch(SessionRequest::authorized(
                    &grant, SessionCommand::Snapshot { run_id: run_id.clone() })).await?;
                let SessionResponse::Snapshot { snapshot } = response else { anyhow::bail!("missing supervised route snapshot"); };
                assert_eq!(snapshot.confirmed_route.as_deref(), Some("goal-model"));
            }
            let serialized = serde_json::to_value(&completed)?;
            let round_trip: GoalSnapshot = serde_json::from_value(serialized)?;
            assert_eq!(round_trip.workflow.tasks, completed.workflow.tasks);
        }
        if scenario == Scenario::Resources {
            let left = completed.workflow.tasks.iter().find(|task| task.spec.id == "left").context("left task missing")?;
            let prepared = left.executions.last().and_then(|execution| execution.resources.as_ref()).context("persistent resource material missing")?;
            assert_eq!(prepared.selection.mcp_servers, vec!["selected-resource-server"]);
            assert_eq!(prepared.selection.context_files, vec!["seed.txt"]);
            assert_eq!(prepared.selection.skills.len(), 1);
            assert_eq!(prepared.materials.len(), 2);
            let prompt = String::from_utf8(crate::goal_artifacts::read(&root, &prepared.prompt).await?)?;
            assert!(prompt.contains("GOAL_RESOURCE_SKILL_MARKER") && prompt.contains("original input"));
            assert!(!serde_json::to_string(&completed)?.contains("private-mcp-fixture-token"));
            let mut transmitted = false;
            let mut captures = tokio::fs::read_dir(&captures).await?;
            while let Some(file) = captures.next_entry().await? {
                let messages = tokio::fs::read_to_string(file.path()).await?.lines()
                    .map(serde_json::from_str::<serde_json::Value>).collect::<std::result::Result<Vec<_>,_>>()?;
                let chosen = messages.iter().find(|message| message["method"] == "session/prompt")
                    .and_then(|message| message["params"]["prompt"][0]["text"].as_str())
                    .is_some_and(|text| text == prompt);
                if chosen {
                    let session = messages.iter().find(|message| message["method"] == "session/new").context("session request missing")?;
                    assert_eq!(session["params"]["mcpServers"].as_array().map(Vec::len), Some(1));
                    assert_eq!(session["params"]["mcpServers"][0]["name"], "selected-resource-server");
                    assert_eq!(session["params"]["mcpServers"][0]["env"][0]["value"], "private-mcp-fixture-token");
                    transmitted = true;
                }
            }
            assert!(transmitted, "persisted prompt/selected descriptor did not reach the actual ACP wire");
            let serialized = serde_json::to_value(&completed)?;
            let round_trip: GoalSnapshot = serde_json::from_value(serialized)?;
            assert_eq!(round_trip.workflow.tasks, completed.workflow.tasks);
        }
        Ok(())
    }.await;
    runtime.shutdown().await;
    supervisor.shutdown().await;
    ipc.abort();
    let _ = ipc.await;
    result
}
