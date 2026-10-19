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

mod live_input;
mod start_recovery;

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
        if tokio::time::Instant::now() >= deadline {
            let mut permissions = Vec::new();
            for decision in &goal.workflow.decisions {
                let bytes = tokio::fs::read(&decision.request.path).await?;
                permissions.push(serde_json::json!({
                    "material": crate::goal_permission_material::checked_value(&bytes, Some(&decision.run_id))?,
                    "effective_tool": crate::goal_permission_material::effective_tool(&bytes, &decision.run_id)?,
                    "state": decision.state,
                }));
            }
            anyhow::bail!(
                "fixture Goal did not reach expected state; execution diagnostics: {}; permission diagnostics: {}; completion diagnostics: {}",
                serde_json::to_string(&goal.workflow.tasks.iter().map(|task| serde_json::json!({"task":task.spec.id,
                    "executions":task.executions.iter().map(|execution| serde_json::json!({"id":execution.id,
                        "observation":execution.observation,"stop":execution.stop,"reconciliation":execution.reconciliation,
                        "review":execution.review,"handoff":execution.handoff})).collect::<Vec<_>>()
                })).collect::<Vec<_>>())?, serde_json::to_string(&permissions)?,
                serde_json::to_string(&serde_json::json!({"completion":goal.workflow.completion,
                    "input":goal.inputs.latest,"budget":goal.budget,"accounting":goal.accounting}))?
            );
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "fixture Goal did not reach expected state: workflow {:?}; accounting {:?}; budget {:?}",
            goal.workflow,
            goal.accounting,
            goal.budget
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_goal_runs_parallel_acp_handoff_input_and_verified_delivery() -> Result<()> {
    parallel_fixture(Scenario::Complete).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmed_pre_session_failure_retries_without_replaying_goal_work() -> Result<()> {
    parallel_fixture(Scenario::StartupRetry).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pre_session_recovery_exhausts_its_bound_without_prompt_or_delivery() -> Result<()> {
    parallel_fixture(Scenario::StartupExhaustion).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_pause_prevents_pre_session_recovery_from_admitting_new_work() -> Result<()> {
    parallel_fixture(Scenario::StartupPause).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sparse_permission_keeps_original_rpc_and_uses_prior_tool_context_for_delivery()
-> Result<()> {
    parallel_fixture(Scenario::PermissionContext).await
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
async fn teardown_denial_has_controller_origin_and_does_not_become_user_consent() -> Result<()> {
    parallel_fixture(Scenario::TeardownDenied).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmed_stop_keeps_original_permission_history_after_cold_reopen() -> Result<()> {
    parallel_fixture(Scenario::ColdTeardown).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_local_failed_tool_can_reconcile_without_a_user_permission_submission()
-> Result<()> {
    parallel_fixture(Scenario::DeniedLocalTerminal).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_branch_edit_preserves_independent_run_and_renews_pending_permission() -> Result<()>
{
    parallel_fixture(Scenario::ActiveEdit).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_input_adds_machine_checks_to_the_actual_replacement_and_delivery() -> Result<()> {
    parallel_fixture(Scenario::CheckEdit).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_review_recovers_from_goal_revision_without_worker_replay() -> Result<()> {
    parallel_fixture(Scenario::ReviewEdit).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_resource_edit_rejects_unknown_choices_and_supplies_the_new_version() -> Result<()> {
    parallel_fixture(Scenario::ResourceEdit).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn known_check_failure_repairs_with_actual_evidence_and_same_requirements() -> Result<()> {
    parallel_fixture(Scenario::CheckRepair).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn known_review_failure_repairs_without_restarting_independent_work() -> Result<()> {
    parallel_fixture(Scenario::ReviewRepair).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_review_failure_stops_at_three_executions_per_version() -> Result<()> {
    parallel_fixture(Scenario::RepairExhaustion).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_review_outcome_never_authorizes_a_repair() -> Result<()> {
    parallel_fixture(Scenario::UnknownReview).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Complete,
    StartupRetry,
    StartupExhaustion,
    StartupPause,
    PermissionContext,
    Resources,
    ModelRoute,
    PauseEdit,
    UnresolvedTool,
    TeardownDenied,
    ColdTeardown,
    DeniedLocalTerminal,
    ActiveEdit,
    CheckEdit,
    ReviewEdit,
    ResourceEdit,
    CheckRepair,
    ReviewRepair,
    RepairExhaustion,
    UnknownReview,
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
        Scenario::Complete
            | Scenario::StartupRetry
            | Scenario::StartupExhaustion
            | Scenario::StartupPause
            | Scenario::ReviewEdit
            | Scenario::Resources
            | Scenario::ModelRoute
            | Scenario::PermissionContext
            | Scenario::CheckRepair
            | Scenario::ReviewRepair
            | Scenario::RepairExhaustion
            | Scenario::UnknownReview
    );
    let active_edit = matches!(
        scenario,
        Scenario::ActiveEdit | Scenario::ResourceEdit | Scenario::CheckEdit
    );
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
    if matches!(scenario, Scenario::Resources | Scenario::ResourceEdit) {
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
    let right_reviews = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let right_review_observations = right_reviews.clone();
    let left_reviews = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            if provider::model_state(request).is_some_and(|state|
                state.get("actual_workspace").is_some() && state["task"]["id"] == "left")
            {
                let attempt = left_reviews.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if scenario == Scenario::UnknownReview {
                    return wiremock::ResponseTemplate::new(500);
                }
                if scenario == Scenario::RepairExhaustion
                    || scenario == Scenario::ReviewRepair && attempt == 0
                {
                    return wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "id":"fixture-rejection","object":"chat.completion","model":"test-model",
                        "choices":[{"index":0,"message":{"role":"assistant","content":
                            serde_json::json!({"ready":false,"reason":"FIXTURE_REPAIR_REASON: a known semantic defect remains","missing_checks":[]}).to_string()},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}));
                }
            }
            let response = provider::model(request);
            if scenario == Scenario::ReviewEdit
                && provider::model_state(request).is_some_and(|state| {
                    state.get("actual_workspace").is_some() && state["task"]["id"] == "right"
                })
                && right_review_observations.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
            {
                response.set_delay(Duration::from_secs(3))
            } else {
                response
            }
        })
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
    let config_text =
        if active_edit || matches!(scenario, Scenario::TeardownDenied | Scenario::ColdTeardown) {
            format!("{config_text}        BRO_GOAL_FIXTURE_RIGHT_MANUAL: '1'\n")
        } else {
            config_text
        };
    let config_text = if matches!(scenario, Scenario::ModelRoute | Scenario::ResourceEdit) {
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
    let config_text = if matches!(scenario, Scenario::StartupRetry | Scenario::StartupPause) {
        format!("{config_text}        BRO_GOAL_FIXTURE_FAIL_FIRST_INITIALIZE: '1'\n")
    } else if scenario == Scenario::StartupExhaustion {
        format!("{config_text}        BRO_GOAL_FIXTURE_FAIL_INITIALIZE: '1'\n")
    } else {
        config_text
    };
    let config_text = if scenario == Scenario::UnresolvedTool {
        format!("{config_text}        BRO_GOAL_FIXTURE_OMIT_TOOL_COMPLETION: '1'\n")
    } else {
        config_text
    };
    let config_text = if scenario == Scenario::CheckRepair {
        format!("{config_text}        BRO_GOAL_FIXTURE_FAIL_LEFT_ONCE: '1'\n")
    } else {
        config_text
    };
    let config_text = if scenario == Scenario::PermissionContext {
        format!("{config_text}        BRO_GOAL_FIXTURE_SPARSE_PERMISSION: '1'\n")
    } else {
        config_text
    };
    let config_text = if scenario == Scenario::DeniedLocalTerminal {
        format!("{config_text}        BRO_GOAL_FIXTURE_RIGHT_DENIED_LOCAL: '1'\n")
    } else {
        config_text
    };
    let config_text = if matches!(scenario, Scenario::Resources | Scenario::ResourceEdit) {
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
    let socket = if matches!(scenario, Scenario::ModelRoute | Scenario::ResourceEdit) {
        crate::daemon::socket_path_for(&source, &config)
    } else {
        home.join("goal.sock")
    };
    let routing = Arc::new(ConfigRoutingTable::from_config(config));
    let supervisor = Supervisor::open(source.clone(), routing.clone()).await?;
    let gate = crate::daemon_handoff::HandoffGate::default();
    let runtime = GoalRuntime::start(
        app.clone(),
        goals.clone(),
        supervisor.clone(),
        routing.clone(),
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
        if scenario == Scenario::StartupExhaustion {
            let exhausted = wait_for(&socket, &id, |goal| goal.workflow.tasks.iter()
                .filter(|task| task.spec.dependencies.is_empty()).count() == 2
                && goal.workflow.tasks.iter().filter(|task| task.spec.dependencies.is_empty())
                .all(|task| task.executions.len() == 3 && task.executions.last()
                    .is_some_and(|execution| execution.effects_reconciled()))).await?;
            tokio::time::sleep(Duration::from_millis(2_100)).await;
            let held = head(&socket, &id).await?;
            assert_eq!(held.requirements_revision, 1);
            assert_eq!(held.run_intent, RunIntent::Run);
            assert_eq!(held.workflow.tasks.iter().map(|task| task.executions.len()).sum::<usize>(), 6);
            assert!(!held.workflow.completion.completed_for(1));
            assert!(held.workflow.tasks.iter().filter(|task| !task.spec.dependencies.is_empty())
                .all(|task| task.executions.is_empty()));
            assert_eq!(held.budget.limits, exhausted.budget.limits);
            let mut files = tokio::fs::read_dir(&captures).await?;
            let mut starts = 0;
            while let Some(file) = files.next_entry().await? {
                let text = tokio::fs::read_to_string(file.path()).await?;
                for line in text.lines() {
                    let message: serde_json::Value = serde_json::from_str(line)?;
                    starts += usize::from(message["method"] == "initialize");
                    assert_ne!(message["method"], "session/new");
                    assert_ne!(message["method"], "session/prompt");
                }
            }
            assert_eq!(starts, 6);
            return Ok(());
        }
        if scenario == Scenario::StartupPause {
            let failed = wait_for(&socket, &id, |goal| goal.workflow.tasks.iter()
                .flat_map(|task| &task.executions).any(|execution| matches!(execution.observation,
                    Observation::Unknown { run_id: Some(_), .. } | Observation::Failed { run_id: Some(_), .. }))).await?;
            let failed_id = failed.workflow.tasks.iter().flat_map(|task| &task.executions)
                .find(|execution| matches!(execution.observation, Observation::Unknown { run_id: Some(_), .. }
                    | Observation::Failed { run_id: Some(_), .. })).context("failed execution missing")?.id.clone();
            control(&socket, &id, "user-pause-before-retry", RunIntent::Pause).await?;
            let paused = wait_for(&socket, &id, |goal| goal.run_intent == RunIntent::Pause
                && goal.workflow.tasks.iter().flat_map(|task| &task.executions)
                    .find(|execution| execution.id == failed_id)
                    .is_some_and(|execution| matches!(execution.stop, Some(StopState::Confirmed { .. })))).await?;
            let attempts = paused.workflow.tasks.iter().map(|task| task.executions.len()).sum::<usize>();
            tokio::time::sleep(Duration::from_millis(2_100)).await;
            let held = head(&socket, &id).await?;
            assert_eq!(held.run_intent, RunIntent::Pause);
            assert_eq!(held.requirements_revision, 1);
            assert_eq!(held.workflow.tasks.iter().map(|task| task.executions.len()).sum::<usize>(), attempts);
            assert!(!held.workflow.completion.completed_for(1));
            return Ok(());
        }
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
        let expected_version = if scenario == Scenario::ReviewEdit {
            let original_right = running.workflow.tasks.iter().find(|task| task.spec.id=="right")
                .and_then(|task| task.executions.last()).context("right execution missing")?.id.clone();
            tokio::fs::write(controls.join("release"), "release original fixture work").await?;
            wait_for(&socket, &id, |goal| right_reviews.load(std::sync::atomic::Ordering::SeqCst)>0
                && goal.workflow.tasks.iter().find(|task| task.spec.id=="right")
                    .and_then(|task| task.executions.last()).is_some_and(|execution| matches!(execution.review,
                        Some(bitrouter_orchestrator::goal::workflow::ReviewState::Requested { .. })))).await?;
            let mut accepted=false;
            for _ in 0..8 {
                let current=head(&socket,&id).await?;
                match rpc(&socket,GoalCommand::Input { operation_id:"edit-during-independent-review".into(),goal_id:id.clone(),
                    expected_revision:current.revision,text:"FIXTURE_NEW_LEFT_RULE: preserve LEFT newline and update only left; the independent right candidate must retain its execution and be assessed in the current Goal context.".into(),task_reference:Some("left".into()) }).await {
                    Ok(GoalReply::Accepted {..})=> {accepted=true;break;},
                    Err(error) if error.downcast_ref::<bitrouter_orchestrator::goal::GoalError>()==Some(&bitrouter_orchestrator::goal::GoalError::Conflict)=>{},
                    other=>anyhow::bail!("review-race edit not admitted: {other:?}"),
                }
            }
            assert!(accepted);
            let edited=wait_for(&socket,&id,|goal| goal.requirements_revision==2).await?;
            let right=edited.workflow.tasks.iter().find(|task|task.spec.id=="right").context("independent task missing")?;
            assert_eq!(right.executions.len(),1);
            assert_eq!(right.executions[0].id,original_right);
            assert_eq!(right.requirements_revision,1);
            2
        } else if pause_edit {
            if matches!(scenario, Scenario::TeardownDenied | Scenario::ColdTeardown | Scenario::DeniedLocalTerminal) {
                let unanswered = wait_for(&socket, &id, |goal| goal.workflow.decisions.iter().any(|decision|
                    decision.state == bitrouter_orchestrator::goal::decision::PermissionState::Pending)).await?;
                let decision = unanswered.workflow.decisions.iter().find(|decision| decision.state == bitrouter_orchestrator::goal::decision::PermissionState::Pending)
                    .context("original unanswered decision missing")?.clone();
                control(&socket, &id, "teardown-pause", RunIntent::Pause).await?;
                let stopped = wait_for(&socket, &id, |goal| goal.workflow.tasks.iter().filter(|task| !task.executions.is_empty())
                    .all(|task| task.executions.last().is_some_and(|execution| matches!(execution.stop, Some(StopState::Confirmed { .. }))))).await?;
                wait_for(&socket, &id, |goal| goal.workflow.decisions.iter().any(|current|
                    current.id == decision.id && matches!(current.state, bitrouter_orchestrator::goal::decision::PermissionState::Obsolete { .. }))).await?;
                let SessionResponse::Authorized { grant } = supervisor.dispatch(SessionRequest::authorize(
                    "teardown-evidence".into(), BTreeSet::from([crate::supervisor::SessionScope::Transcript]),
                )).await? else { anyhow::bail!("teardown transcript grant missing") };
                let SessionResponse::Events { replay } = supervisor.dispatch(SessionRequest::authorized(
                    &grant, SessionCommand::Events { run_id: decision.run_id.clone(), after_seq: 0 },
                )).await? else { anyhow::bail!("teardown transcript missing") };
                let resolution = replay.events.iter().find(|event| matches!(&event.kind,
                    crate::supervisor::SessionEventKind::PermissionResolved { permission_id, .. } if permission_id == &decision.permission_id))
                    .context("actual rejection missing")?;
                let value = serde_json::to_value(&resolution.kind)?;
                assert_eq!(value["option_id"], "deny-once");
                assert_eq!(value["origin"], "teardown");
                if scenario == Scenario::DeniedLocalTerminal {
                    let original = decision.execution_id.clone();
                    assert!(!controls.join("completed-right").exists());
                    assert!(matches!(decision.state, bitrouter_orchestrator::goal::decision::PermissionState::Pending));
                    control(&socket, &id, "explicit-resume-denied-local", RunIntent::Run).await?;
                    let reconciled = wait_for(&socket, &id, |goal| goal.workflow.tasks.iter().flat_map(|task| &task.executions)
                        .find(|execution| execution.id == original).is_some_and(|execution| matches!(execution.reconciliation,
                            Some(bitrouter_orchestrator::goal::workflow::LocalReconciliation::Verified { .. })))).await?;
                    let execution = reconciled.workflow.tasks.iter().flat_map(|task| &task.executions)
                        .find(|execution| execution.id == original).context("original denied execution missing")?;
                    let Some(bitrouter_orchestrator::goal::workflow::LocalReconciliation::Verified { transcript, .. }) = &execution.reconciliation
                        else { anyhow::bail!("local failed terminal not reconciled") };
                    let root = crate::goal_artifacts::root(&home.join("goal-workspaces"), &id);
                    let proof: serde_json::Value = serde_json::from_slice(&crate::goal_artifacts::read(&root, transcript).await?)?;
                    assert_eq!(proof["permission_submissions"].as_array().map(Vec::len), Some(0));
                    assert_eq!(proof["controller_denials"].as_array().map(Vec::len), Some(1));
                    assert!(!reconciled.workflow.completion.completed_for(1));
                    return Ok(());
                }
                if scenario == Scenario::ColdTeardown {
                    let cold = Supervisor::open(source.clone(), routing.clone()).await?;
                    let SessionResponse::Authorized { grant } = cold.dispatch(SessionRequest::authorize(
                        "cold-teardown-reader".into(), BTreeSet::from([crate::supervisor::SessionScope::Transcript, crate::supervisor::SessionScope::Peek]),
                    )).await? else { anyhow::bail!("cold transcript grant missing") };
                    let SessionResponse::Events { replay: restored } = cold.dispatch(SessionRequest::authorized(
                        &grant, SessionCommand::Events { run_id: decision.run_id.clone(), after_seq: 0 },
                    )).await? else { anyhow::bail!("cold stopped transcript missing") };
                    assert!(restored.history_complete);
                    assert_eq!(serde_json::to_value(&restored)?, serde_json::to_value(&replay)?);
                    let SessionResponse::Snapshot { snapshot } = cold.dispatch(SessionRequest::authorized(
                        &grant, SessionCommand::Snapshot { run_id: decision.run_id.clone() },
                    )).await? else { anyhow::bail!("cold stopped snapshot missing") };
                    assert_eq!(snapshot.process, ProcessState::Stopped);
                    assert_eq!(snapshot.last_seq, replay.snapshot_seq);
                }
                assert_eq!(stopped.run_intent, RunIntent::Pause);
                assert!(!stopped.workflow.completion.completed_for(1));
                assert_eq!(stopped.workflow.tasks.iter().map(|task| task.executions.len()).sum::<usize>(), 2);
                return Ok(());
            }
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
            if scenario == Scenario::ResourceEdit {
                let current = head(&socket, &id).await?;
                rpc(&socket, GoalCommand::Input { operation_id:"invalid-resource-change".into(),goal_id:id.clone(),
                    expected_revision:current.revision,text:"FIXTURE_NEW_LEFT_RULE: BAD_RESOURCE_CHANGE_FIXTURE select an unavailable server; preserve all other work.".into(),task_reference:Some("left".into()) }).await?;
                let rejected = wait_for(&socket,&id,|goal|goal.inputs.latest.as_ref().is_some_and(|input|input.input_id == "invalid-resource-change")).await?;
                let result = rejected.inputs.latest.as_ref().context("missing resource rejection")?;
                assert!(result.clarification.as_ref().is_some_and(|text|text.contains("unavailable configured MCP")));
                assert!(!result.requirements_saved && result.control_saved.is_none());
                assert_eq!(rejected.inputs.unresolved_inputs().len(),1);
                assert!(rejected.inputs.completion_waits_for_input());
                assert_eq!(rejected.requirements_revision,1);
                assert!(rejected.workflow.tasks.iter().all(|task|task.spec.resources.is_empty()));
                assert_eq!(rejected.workflow.tasks.iter().map(|task|task.executions.len()).sum::<usize>(),2);
            }
            let mut accepted = false;
            for _ in 0..8 {
                let current = head(&socket,&id).await?;
                match rpc(&socket,GoalCommand::Input { operation_id:"edit-while-paused".into(),goal_id:id.clone(),
                    expected_revision:current.revision,text: if scenario == Scenario::ResourceEdit {
                        "FIXTURE_NEW_LEFT_RULE: RESOURCE_CHANGE_FIXTURE: keep LEFT newline, select the available goal-model route, resource server, goal resource skill and seed context for this branch's next execution. Preserve independent work.".into()
                    } else if scenario == Scenario::CheckEdit {
                        "FIXTURE_NEW_LEFT_RULE: CHECK_CHANGE_FIXTURE: retain the original check and add required executable verification that seed.txt contains original input.".into()
                    } else { "FIXTURE_NEW_LEFT_RULE: keep LEFT newline and use these updated instructions when left executes again; do not resume.".into() },task_reference:Some("left".into()) }).await {
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
                let response = GoalCommand::RespondDecision { operation_id:"current-right-response".into(),goal_id:id.clone(),decision_id:decision.id.clone(),
                    expected_requirements_revision:2,option_id:"allow-once".into() };
                let mut accepted = false;
                // Background accounting may win the head CAS. Retry only a known
                // rejection with the same operation, source version and permission.
                for _ in 0..8 {
                    match rpc(&socket, response.clone()).await {
                        Ok(GoalReply::Accepted { operation }) => {
                            assert_eq!(operation.id, "current-right-response");
                            assert_eq!(operation.requirements_revision, 2);
                            accepted = true;
                            break;
                        },
                        Err(error) if error.downcast_ref::<bitrouter_orchestrator::goal::GoalError>()
                            == Some(&bitrouter_orchestrator::goal::GoalError::Conflict) => {},
                        other => anyhow::bail!("current fixture permission was not admitted: {other:?}"),
                    }
                }
                assert!(accepted, "current permission remained in conflict");
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
        if matches!(scenario, Scenario::RepairExhaustion | Scenario::UnknownReview) {
            let limit = if scenario == Scenario::RepairExhaustion { 3 } else { 1 };
            let blocked = wait_for(&socket, &id, |goal| goal.workflow.tasks.iter()
                .find(|task| task.spec.id == "left").is_some_and(|task|
                    task.executions.len() == limit && task.executions.last().is_some_and(|execution|
                        matches!(execution.review, Some(ReviewState::Rejected { .. }))
                        && (scenario == Scenario::UnknownReview || execution.effects_reconciled())))).await?;
            tokio::time::sleep(Duration::from_millis(1_100)).await;
            let held = head(&socket, &id).await?;
            assert_eq!(held.requirements_revision, 1);
            assert_eq!(held.run_intent, RunIntent::Run);
            assert_eq!(held.budget.limits, blocked.budget.limits);
            assert_eq!(held.workflow.tasks.iter().find(|task| task.spec.id == "left")
                .context("left missing")?.executions.len(), limit);
            assert!(held.workflow.tasks.iter().find(|task| task.spec.id == "join")
                .context("join missing")?.executions.is_empty());
            assert!(!held.workflow.completion.completed_for(1));
            return Ok(());
        }
        let completed = wait_for(&socket,&id,|goal| goal.workflow.completion.completed_for(expected_version)).await?;
        let delivery = completed.workflow.completion.last_completed.as_ref().context("missing verified delivery")?;
        assert_eq!(delivery.requirements_revision,expected_version);
        assert!(!workspace.join("left.txt").exists());
        assert!(!workspace.join("right.txt").exists());
        assert!(controls.join("started-join").exists());
        let root = crate::goal_artifacts::root(&home.join("goal-workspaces"),&id);
        let joined = completed.workflow.tasks.iter().find(|task| task.spec.id == "join")
            .and_then(|task| task.executions.last()).context("joined execution missing")?;
        let prepared = joined.resources.as_ref().context("joined input missing")?;
        let reference = prepared.checked_dependencies.as_ref().context("dependency reports were not frozen before dispatch")?;
        let supplied = crate::goal_artifacts::read(&root, reference).await?;
        let dependencies: serde_json::Value = serde_json::from_slice(&supplied)?;
        assert_eq!(dependencies.as_array().context("dependencies not an array")?.len(), 2);
        let prompt = String::from_utf8(crate::goal_artifacts::read(&root, &prepared.prompt).await?)?;
        assert!(prompt.contains(std::str::from_utf8(&supplied)?));
        if matches!(scenario, Scenario::CheckRepair | Scenario::ReviewRepair) {
            let left = completed.workflow.tasks.iter().find(|task| task.spec.id == "left").context("left missing")?;
            assert_eq!(left.requirements_revision, 1);
            assert_eq!(left.executions.len(), 2);
            assert!(left.executions[0].effects_reconciled());
            assert!(left.executions[0].handoff.is_none());
            assert_eq!(completed.workflow.tasks.iter().find(|task| task.spec.id == "right")
                .context("right missing")?.executions.len(), 1);
            let prepared = left.executions[1].resources.as_ref().context("repair prompt missing")?;
            let prompt = String::from_utf8(crate::goal_artifacts::read(&root, &prepared.prompt).await?)?;
            assert!(prompt.contains(&left.executions[0].id));
            assert!(prompt.contains("failed_candidate_material") && prompt.contains("actual_checks"));
            assert!(prompt.contains(if scenario == Scenario::CheckRepair { "WRONG" } else { "FIXTURE_REPAIR_REASON" }));
            assert_eq!(completed.budget.limits, running.budget.limits);
        }
        if scenario == Scenario::ReviewEdit {
            assert!(right_reviews.load(std::sync::atomic::Ordering::SeqCst)>=2);
            let right=completed.workflow.tasks.iter().find(|task|task.spec.id=="right").context("reviewed right task missing")?;
            assert_eq!(right.executions.len(),1);
            assert_eq!(right.requirements_revision,1);
            let handoff=right.executions[0].handoff.as_ref().context("fresh independent handoff missing")?;
            assert_eq!(handoff.requirements_revision,2);
            assert_eq!(handoff.task_revision,1);
        }
        if scenario == Scenario::CheckEdit {
            let left=completed.workflow.tasks.iter().find(|task|task.spec.id=="left").context("updated checked task missing")?;
            assert_eq!(left.spec.checks.len(),2);
            assert!(left.spec.checks.iter().any(|check|check.id=="left-written"));
            let extra=left.spec.checks.iter().find(|check|check.id=="left-keeps-seed").context("additional check absent from executable spec")?;
            let execution=left.executions.last().context("updated execution missing")?;
            let actual=execution.checks.iter().find(|check|check.spec==*extra).context("additional check was not actually dispatched")?;
            assert_eq!(actual.task_revision,2);
            assert!(matches!(actual.state,bitrouter_orchestrator::goal::workflow::CheckState::Observed {passed:true,completed:true,..}));
        }
        let material = crate::goal_artifacts::material(&root,&delivery.delivery).await?;
        let material = serde_json::to_string(&material)?;
        assert!(material.contains("left.txt") && material.contains("right.txt") && material.contains("joined.txt"));
        assert!(completed.workflow.tasks.iter().all(|task| task.executions.last().is_some_and(|execution|
            execution.handoff.is_some() && matches!(execution.stop,Some(StopState::Confirmed { .. })))));
        let requests = backend.received_requests().await.context("fixture request capture unavailable")?;
        if matches!(scenario, Scenario::CheckRepair | Scenario::ReviewRepair | Scenario::ActiveEdit | Scenario::CheckEdit) {
            let assessment = requests.iter().filter_map(provider::model_state)
                .find(|state| state.get("integrated_material").is_some())
                .context("final evidence request missing")?;
            let left = assessment["task_evidence"].as_array().context("task evidence missing")?.iter()
                .find(|task| task["task"]["id"] == "left").context("left history missing")?;
            let history = &left["historical_runtime_observations"];
            assert_eq!(history["historical_only"], true);
            assert_eq!(history["current_artifact_authority"], false);
            assert!(history["entries"].as_array().is_some_and(|entries| entries.iter()
                .any(|entry| entry["status"] == "observed" && entry["admission_is_execution_evidence"] == false)));
            if matches!(scenario, Scenario::ActiveEdit | Scenario::CheckEdit) {
                assert!(history["entries"].as_array().is_some_and(|entries| entries.iter()
                    .any(|entry| entry["committed_archive"].is_object() && entry["archive_operation_id"].is_string())));
            }
        }
        assert!(requests.iter().any(|request| request.url.path() == "/v1/systemone"));
        assert!(requests.iter().filter(|request| request.url.path() == "/v1/chat/completions").count() >= 6);
        if scenario == Scenario::ResourceEdit {
            assert!(completed.inputs.unresolved_inputs().is_empty());
            assert!(completed.inputs.latest.as_ref().is_some_and(|input| input.resolutions.iter().any(|resolution| resolution.input_id == "invalid-resource-change")));
            let completed = wait_for(&socket,&id,|goal|goal.workflow.tasks.iter().find(|task|task.spec.id == "left").is_some_and(|task|task.archived_executions == 1)).await?;
            let left = completed.workflow.tasks.iter().find(|task|task.spec.id == "left").context("left task missing")?;
            assert_eq!(left.executions.len(),1);
            let pointer = left.latest_archive.as_ref().context("readonly archive pointer missing")?;
            let metadata: serde_json::Value = serde_json::from_slice(&crate::goal_artifacts::read(&root,pointer).await?)?;
            let original_id = metadata["execution"]["id"].as_str().context("archive original identity missing")?;
            let archive = rpc(&socket,GoalCommand::Operation { operation_id: bitrouter_orchestrator::goal::workflow::archive_operation_id(&id,original_id) }).await?;
            let GoalReply::Accepted { operation } = archive else { anyhow::bail!("missing cold original execution receipt") };
            let bitrouter_orchestrator::goal::OperationKind::Workflow { change } = operation.kind else { anyhow::bail!("unexpected archive receipt") };
            let Change::ArchiveExecution { execution: old, record, .. } = *change else { anyhow::bail!("missing complete original execution") };
            assert_eq!(&record,pointer);
            assert_eq!(serde_json::from_value::<Execution>(metadata["execution"].clone())?,*old);
            let inspected = crate::goal_code::read_archive_fixture(&socket,&root,&id,"left",pointer).await?;
            assert_eq!(inspected.title,format!("Archived execution {}",old.id));
            assert!(inspected.details.iter().any(|text|text.contains(&old.id)));
            assert!(inspected.artifacts.iter().any(|artifact|artifact.label.contains("Original tool/stop reconciliation")));
            let replacement = left.executions.last().context("replacement missing")?;
            assert!(old.resources.as_ref().is_some_and(|prepared|prepared.selection.is_empty()));
            assert!(matches!(old.stop,Some(StopState::Confirmed {..})));
            assert_eq!(replacement.task_revision,2);
            assert_eq!(replacement.confirmed_model_route.as_deref(),Some("goal-model"));
            let prepared = replacement.resources.as_ref().context("new resource preparation missing")?;
            assert_eq!(prepared.selection.mcp_servers,vec!["selected-resource-server"]);
            assert_eq!(prepared.selection.context_files,vec!["seed.txt"]);
            assert_eq!(prepared.selection.skills.len(),1);
            assert_eq!(prepared.materials.len(),2);
            let prompt = String::from_utf8(crate::goal_artifacts::read(&root,&prepared.prompt).await?)?;
            assert!(prompt.contains("GOAL_RESOURCE_SKILL_MARKER") && prompt.contains("original input"));
            let mut matched = false;
            let mut files = tokio::fs::read_dir(&captures).await?;
            while let Some(file) = files.next_entry().await? {
                let text = tokio::fs::read_to_string(file.path()).await?;
                let messages = text.lines().map(serde_json::from_str::<serde_json::Value>).collect::<std::result::Result<Vec<_>,_>>()?;
                if messages.iter().any(|message|message["method"] == "session/prompt" && message["params"]["prompt"][0]["text"] == prompt) {
                    let opened = messages.iter().find(|message|message["method"] == "session/new").context("new session missing")?;
                    assert_eq!(opened["params"]["mcpServers"].as_array().map(Vec::len),Some(1));
                    assert_eq!(opened["params"]["mcpServers"][0]["name"],"selected-resource-server");
                    matched = true;
                }
            }
            assert!(matched,"new frozen material did not reach the replacement ACP prompt");
            let right = completed.workflow.tasks.iter().find(|task|task.spec.id == "right").context("right task missing")?;
            assert_eq!(right.executions.len(),1);
            assert_eq!(right.requirements_revision,1);
            assert!(right.spec.resources.is_empty());
        }
        if scenario == Scenario::StartupRetry {
            let attempts = completed.workflow.tasks.iter().flat_map(|task| &task.executions).collect::<Vec<_>>();
            assert_eq!(attempts.len(), 4, "one startup failed, three real task prompts remain");
            let failed = attempts.iter().filter(|execution| matches!(execution.observation,
                Observation::Unknown { .. } | Observation::Failed { .. })).collect::<Vec<_>>();
            assert_eq!(failed.len(), 1);
            assert!(failed[0].effects_reconciled());
            let Some(bitrouter_orchestrator::goal::workflow::LocalReconciliation::Verified { transcript, .. }) = &failed[0].reconciliation
                else { anyhow::bail!("original startup reconciliation missing") };
            let evidence: serde_json::Value = serde_json::from_slice(&crate::goal_artifacts::read(&root, transcript).await?)?;
            assert_eq!(evidence["snapshot"]["process"], "failed");
            assert_eq!(evidence["snapshot"]["startup_aborted_before_session"], true);
            assert!(evidence["history"]["history_complete"].as_bool() == Some(true));
            let mut files = tokio::fs::read_dir(&captures).await?;
            let (mut failed_starts, mut prompts) = (0, 0);
            while let Some(file) = files.next_entry().await? {
                let text = tokio::fs::read_to_string(file.path()).await?;
                let messages = text.lines().map(serde_json::from_str::<serde_json::Value>)
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let opened = messages.iter().any(|message| message["method"] == "session/new");
                let admitted = messages.iter().filter(|message| message["method"] == "session/prompt").count();
                prompts += admitted;
                if !opened {
                    assert_eq!(admitted, 0);
                    failed_starts += 1;
                }
            }
            assert_eq!((failed_starts, prompts), (1, 3));
        }
        if scenario == Scenario::PermissionContext {
            use bitrouter_orchestrator::goal::decision::Event;
            let mut requests = BTreeMap::new();
            let mut delivered = BTreeSet::new();
            let mut after = 0;
            loop {
                let GoalReply::History { page } = rpc(&socket, GoalCommand::History {
                    goal_id: id.clone(), after, cutoff: Some(completed.revision), limit: 16,
                }).await? else { anyhow::bail!("original permission history missing") };
                assert_eq!(page.cutoff, completed.revision);
                for operation in page.operations {
                    if let bitrouter_orchestrator::goal::OperationKind::Workflow { change } = operation.kind
                        && let Change::Decision { event } = *change {
                        match event {
                            Event::Observe { decision } => { requests.insert(decision.id.clone(), decision); },
                            Event::Result { decision_id, submitted: true, .. } => { delivered.insert(decision_id); },
                            _ => {},
                        }
                    }
                }
                let Some(next) = page.next_after else { break };
                assert!(next > after);
                after = next;
            }
            assert_eq!(requests.len(), 3);
            assert_eq!(delivered, requests.keys().cloned().collect());
            let SessionResponse::Authorized { grant } = supervisor.dispatch(SessionRequest::authorize(
                "permission-context-evidence".into(), BTreeSet::from([crate::supervisor::SessionScope::Transcript]),
            )).await? else { anyhow::bail!("permission transcript grant missing") };
            for decision in requests.values() {
                let bytes = crate::goal_artifacts::read(&root, &decision.request).await?;
                let request: crate::supervisor::PendingPermissionSnapshot = serde_json::from_slice(&bytes)?;
                let tool = serde_json::to_value(&request.tool_call)?;
                assert_eq!(tool["locations"], serde_json::json!([]));
                assert_eq!(tool["rawInput"], serde_json::json!({}));
                let material = crate::goal_permission_material::checked_value(&bytes, Some(&decision.run_id))?;
                assert!(material["earlier_tool_context"]["declarations"].as_array().is_some_and(|events| events.len() == 2));
                let SessionResponse::Events { replay } = supervisor.dispatch(SessionRequest::authorized(
                    &grant, SessionCommand::Events { run_id: decision.run_id.clone(), after_seq: 0 },
                )).await? else { anyhow::bail!("original controller transcript missing") };
                let original = replay.events.iter().find_map(|event| match &event.kind {
                    crate::supervisor::SessionEventKind::Permission { permission } if permission.permission_id == decision.permission_id => Some(permission),
                    _ => None,
                }).context("original controller request missing")?;
                assert_eq!(crate::goal_permission_material::original_bytes(&bytes)?, serde_json::to_vec(original)?);
            }
        }
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
