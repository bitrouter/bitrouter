//! Full coordinator transport tests. Provider judgments are explicit fixtures.
use super::*;
use crate::daemon::{
    AcpControlPlane, DaemonCommand, DaemonResponse, LocalControlOptions, NoopObserveStatus,
    NoopReloader, bind_control_socket, run_bound_control_socket, send_command,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

const RESOURCE_OBJECTIVE: &str = "RESOURCE_SELECTION_FIXTURE: Deliver LEFT and RIGHT files in parallel, then join their actual contents. Configure the applicable MCP server and provide the goal resource skill and seed context to LEFT. BRO must verify all three files. No human acceptance is required.";
const ROUTED_OBJECTIVE: &str = "MODEL_ROUTE_SELECTION_FIXTURE: Deliver LEFT and RIGHT files in parallel, then join their actual contents. Select the available BRO route for every task. BRO must verify all three files. No human acceptance is required.";

#[path = "../../../../crates/bitrouter-orchestrator/tests/support/fixture_binary.rs"]
mod fixture_binary;

pub(super) fn peer_executable() -> Result<PathBuf> {
    static BINARY: std::sync::OnceLock<std::result::Result<PathBuf, String>> =
        std::sync::OnceLock::new();
    BINARY
        .get_or_init(|| fixture_binary::build("goal_acp_fixture"))
        .clone()
        .map_err(anyhow::Error::msg)
}

#[path = "../../tests/support/goal_provider.rs"]
mod provider;

mod fixture;
mod live_input;
mod native;
mod review_barrier;
mod start_recovery;
mod threads;
mod workflow_edits;
mod workflow_permissions;
mod workflow_recovery;

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
                    "material": crate::goal_permission_material::decision_value(&bytes, decision)?,
                    "effective_tool": crate::goal_permission_material::decision_tool(&bytes, decision)?,
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

use fixture::{Fixture, Setup};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_goal_runs_parallel_acp_handoff_input_and_verified_delivery() -> Result<()> {
    let fixture = Setup::new().await?.start().await?;
    let result = async {
        let GoalReply::Directory { page } = rpc(
            &fixture.socket,
            GoalCommand::List {
                after: 0,
                cutoff: None,
                limit: 16,
            },
        )
        .await?
        else {
            anyhow::bail!("cold directory read omitted its page")
        };
        assert!(page.goals.is_empty());
        assert!(
            fixture
                .backend
                .received_requests()
                .await
                .context("fixture request capture unavailable")?
                .is_empty()
        );
        assert!(
            tokio::fs::read_dir(&fixture.captures)
                .await?
                .next_entry()
                .await?
                .is_none()
        );
        let id = fixture.create().await?;
        fixture.running(&id).await?;
        fixture
            .input(
                &id,
                "question-during-work",
                "What is currently active? Do not change anything.",
                None,
            )
            .await?;
        let answered = wait_for(&fixture.socket, &id, |goal| {
            goal.inputs
                .latest
                .as_ref()
                .is_some_and(|input| input.input_id == "question-during-work")
        })
        .await?;
        let input = answered.inputs.latest.context("missing input result")?;
        assert!(input.answer.is_some());
        assert!(!input.requirements_saved);
        assert!(input.control_saved.is_none());
        assert!(input.classification.is_some());
        assert_eq!(answered.requirements_revision, 1);
        assert_eq!(
            answered
                .workflow
                .tasks
                .iter()
                .filter(
                    |task| task.executions.last().is_some_and(|execution| matches!(
                        execution.observation,
                        Observation::Running { .. }
                    ))
                )
                .count(),
            2
        );
        assert!(!answered.workflow.completion.completed_for(1));
        fixture.release().await?;
        let completed = fixture.completed(&id, 1).await?;
        let delivery = completed
            .workflow
            .completion
            .last_completed
            .as_ref()
            .context("verified delivery missing")?;
        let integrated = completed
            .workflow
            .completion
            .attempt
            .as_ref()
            .context("completed integration attempt missing")?
            .workspace
            .canonicalize()?;
        let branch_check_ids = completed
            .workflow
            .tasks
            .iter()
            .flat_map(|task| &task.executions)
            .flat_map(|execution| &execution.checks)
            .map(|check| check.id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(delivery.checks.len(), 3);
        for check in &delivery.checks {
            assert!(!branch_check_ids.contains(check.id.as_str()));
            assert_eq!(check.workspace, delivery.delivery);
            let bitrouter_orchestrator::goal::workflow::CheckState::Observed { report, .. } =
                &check.state
            else {
                anyhow::bail!("integration check outcome missing")
            };
            let output: bitrouter_orchestrator::goal::verification::CheckOutput =
                serde_json::from_slice(
                    &crate::goal_artifacts::read(&fixture.root(&id), report).await?,
                )?;
            assert!(output.supports(&check.spec));
            assert_eq!(
                output.environment["workspace"],
                serde_json::to_value(&integrated)?
            );
        }
        let requests = fixture
            .backend
            .received_requests()
            .await
            .context("fixture request capture unavailable")?;
        assert!(
            requests
                .iter()
                .any(|request| request.url.path() == "/v1/systemone")
        );
        assert!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/v1/chat/completions")
                .count()
                >= 6
        );
        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn goal_selected_resources_are_persisted_and_sent_as_actual_acp_material() -> Result<()> {
    let mut setup = Setup::new().await?;
    setup.resources().await?;
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create_with(RESOURCE_OBJECTIVE).await?;

        fixture.running(&id).await?;
        fixture.release().await?;
        let completed = fixture.completed(&id, 1).await?;
        let root = fixture.root(&id);
        let left = completed
            .workflow
            .tasks
            .iter()
            .find(|task| task.spec.id == "left")
            .context("left task missing")?;
        let prepared = left
            .executions
            .last()
            .and_then(|execution| execution.resources.as_ref())
            .context("persistent resource material missing")?;
        assert_eq!(
            prepared.selection.mcp_servers,
            vec!["selected-resource-server"]
        );
        assert_eq!(prepared.selection.context_files, vec!["seed.txt"]);
        assert_eq!(prepared.selection.skills.len(), 1);
        assert_eq!(prepared.materials.len(), 2);
        let prompt =
            String::from_utf8(crate::goal_artifacts::read(&root, &prepared.prompt).await?)?;
        assert!(prompt.contains("GOAL_RESOURCE_SKILL_MARKER") && prompt.contains("original input"));
        assert!(!serde_json::to_string(&completed)?.contains("private-mcp-fixture-token"));
        let mut transmitted = false;
        let mut captured_files = tokio::fs::read_dir(&fixture.captures).await?;
        while let Some(file) = captured_files.next_entry().await? {
            let messages = tokio::fs::read_to_string(file.path())
                .await?
                .lines()
                .map(serde_json::from_str::<serde_json::Value>)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let chosen = messages
                .iter()
                .find(|message| message["method"] == "session/prompt")
                .and_then(|message| message["params"]["prompt"][0]["text"].as_str())
                .is_some_and(|text| text == prompt);
            if chosen {
                let session = messages
                    .iter()
                    .find(|message| message["method"] == "session/new")
                    .context("session request missing")?;
                assert_eq!(
                    session["params"]["mcpServers"].as_array().map(Vec::len),
                    Some(1)
                );
                assert_eq!(
                    session["params"]["mcpServers"][0]["name"],
                    "selected-resource-server"
                );
                assert_eq!(
                    session["params"]["mcpServers"][0]["env"][0]["value"],
                    "private-mcp-fixture-token"
                );
                transmitted = true;
            }
        }
        assert!(
            transmitted,
            "persisted prompt/selected descriptor did not reach the actual ACP wire"
        );
        let serialized = serde_json::to_value(&completed)?;
        let round_trip: GoalSnapshot = serde_json::from_value(serialized)?;
        assert_eq!(round_trip.workflow.tasks, completed.workflow.tasks);
        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn goal_model_route_is_confirmed_before_prompt_and_persisted_through_delivery() -> Result<()>
{
    let mut setup = Setup::new().await?;
    setup.routed_agent().await;
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create_with(ROUTED_OBJECTIVE).await?;

        fixture.running(&id).await?;
        fixture.release().await?;
        let completed = fixture.completed(&id, 1).await?;
        for task in &completed.workflow.tasks {
            assert_eq!(
                task.spec.resources.model_route.as_deref(),
                Some("goal-model")
            );
            let execution = task
                .executions
                .last()
                .context("missing model route execution")?;
            assert_eq!(
                execution.confirmed_model_route.as_deref(),
                Some("goal-model")
            );
            let Observation::Candidate {
                run_id: Some(run_id),
                ..
            } = &execution.observation
            else {
                anyhow::bail!("confirmed model execution did not produce candidate");
            };
            let SessionResponse::Authorized { grant } = fixture
                .supervisor
                .dispatch(SessionRequest::authorize(
                    "model-route-test".into(),
                    BTreeSet::from([crate::supervisor::SessionScope::Peek]),
                ))
                .await?
            else {
                anyhow::bail!("route inspection grant missing");
            };
            let response = fixture
                .supervisor
                .dispatch(SessionRequest::authorized(
                    &grant,
                    SessionCommand::Snapshot {
                        run_id: run_id.clone(),
                    },
                ))
                .await?;
            let SessionResponse::Snapshot { snapshot } = response else {
                anyhow::bail!("missing supervised route snapshot");
            };
            assert_eq!(snapshot.confirmed_route.as_deref(), Some("goal-model"));
        }
        let serialized = serde_json::to_value(&completed)?;
        let round_trip: GoalSnapshot = serde_json::from_value(serialized)?;
        assert_eq!(round_trip.workflow.tasks, completed.workflow.tasks);
        Ok(())
    }
    .await;
    fixture.finish(result).await
}
