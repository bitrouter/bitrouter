//! Explicit paused, active and in-flight-review edits to the left branch.
use super::*;

const CHECK_EDIT: &str = "FIXTURE_NEW_LEFT_RULE: CHECK_CHANGE_FIXTURE: retain the original check and add required executable verification that seed.txt contains original input.";
const INVALID_RESOURCE_EDIT: &str = "FIXTURE_NEW_LEFT_RULE: BAD_RESOURCE_CHANGE_FIXTURE select an unavailable server; preserve all other work.";
const RESOURCE_EDIT: &str = "FIXTURE_NEW_LEFT_RULE: RESOURCE_CHANGE_FIXTURE: keep LEFT newline, select the available goal-model route, resource server, goal resource skill and seed context for this branch's next execution. Preserve independent work.";
const REVIEW_EDIT: &str = "FIXTURE_NEW_LEFT_RULE: preserve LEFT newline and update only left; the independent right candidate must retain its execution and be assessed in the current Goal context.";

const LEFT_EDIT: &str = "FIXTURE_NEW_LEFT_RULE: keep LEFT newline and use these updated instructions when left executes again; do not resume.";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unconfigured_worker_request_is_retained_as_clarification_without_replacement() -> Result<()>
{
    let fixture = Setup::new().await?.start().await?;
    let result = async {
        let id = fixture.create().await?;
        let before = fixture.running(&id).await?;
        fixture.input(&id,"invalid-worker","FIXTURE_NEW_LEFT_RULE: WORKER_SWITCH_MISSING_FIXTURE: use this unavailable worker only for left.",Some("left")).await?;
        let rejected = wait_for(&fixture.socket,&id,|goal| goal.inputs.latest.as_ref().is_some_and(|input| input.input_id == "invalid-worker")).await?;
        let result = rejected.inputs.latest.as_ref().context("worker rejection missing")?;
        assert!(!result.requirements_saved);
        assert!(result.clarification.as_ref().is_some_and(|message| message.contains("not configured")));
        assert_eq!(rejected.requirements_revision,before.requirements_revision);
        assert_eq!(task(&rejected,"left")?.spec.harness,task(&before,"left")?.spec.harness);
        assert_eq!(task(&rejected,"left")?.executions[0].thread,task(&before,"left")?.executions[0].thread);
        assert_eq!(rejected.workflow.tasks.iter().map(|task| task.executions.len()).sum::<usize>(),2);
        assert!(rejected.inputs.completion_waits_for_input());
        Ok(())
    }.await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requested_new_worker_cannot_bypass_unknown_old_tool_effects_or_resume_a_pause()
-> Result<()> {
    let mut setup = Setup::new().await?;
    setup.agent_env("BRO_GOAL_FIXTURE_OMIT_TOOL_COMPLETION");
    setup.worker_alias("replacement-peer")?;
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;
        fixture.running(&id).await?;
        let paused = paused_after_workers(&fixture,&id).await?;
        let old = task(&paused,"left")?.executions[0].clone();
        let edited = edit_left(&fixture,&id,"FIXTURE_NEW_LEFT_RULE: WORKER_SWITCH_ACP_FIXTURE: select the replacement worker for left and retain the current pause.").await?;
        assert_eq!(edited.run_intent,RunIntent::Pause);
        assert_eq!(task(&edited,"left")?.spec.harness.as_deref(),Some("replacement-peer"));
        control(&fixture.socket,&id,"resume-worker-request",RunIntent::Run).await?;
        let blocked = wait_for(&fixture.socket,&id,|goal| task(goal,"left").is_ok_and(|task| task.executions[0].reconciliation.as_ref().is_some_and(|state| matches!(state, bitrouter_orchestrator::goal::workflow::LocalReconciliation::Blocked { .. })))).await?;
        let left = task(&blocked,"left")?;
        assert_eq!(left.executions.len(),1);
        assert_eq!(left.executions[0].id,old.id);
        assert_eq!(left.executions[0].thread,old.thread);
        assert_eq!(left.execution_harness(&left.executions[0]),Some("goal-peer"));
        assert!(!blocked.workflow.completion.completed_for(2));
        assert!(!fixture.controls.join("started-join").exists());
        Ok(())
    }.await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requested_worker_replacement_keeps_old_session_source_and_independent_work() -> Result<()>
{
    use bitrouter_orchestrator::thread::ThreadTarget;
    for native in [false, true] {
        let mut setup = Setup::new().await?;
        setup.agent_env("BRO_GOAL_FIXTURE_RIGHT_MANUAL");
        setup.worker_alias("replacement-peer")?;
        if native {
            Mock::given(method("POST")).and(path("/v1/chat/completions"))
                .respond_with(|request: &wiremock::Request| {
                    let body: serde_json::Value = match serde_json::from_slice(&request.body) {
                        Ok(body) => body,
                        Err(_) => return wiremock::ResponseTemplate::new(400),
                    };
                    if body["tools"].as_array().is_none_or(Vec::is_empty) {
                        return provider::model(request);
                    }
                    let messages = body["messages"].as_array().into_iter().flatten().collect::<Vec<_>>();
                    let written = messages.iter().any(|message| message["role"] == "tool" && message["tool_call_id"] == "native-left-write");
                    if written {
                        return super::native::completion(&body,serde_json::json!({"role":"assistant","content":"Inherited LEFT newline was read and retained; BRO must verify actual files."}),"stop");
                    }
                    let inherited = messages.iter().find(|message| message["role"] == "tool" && message["tool_call_id"] == "native-left-read");
                    let message = if let Some(inherited) = inherited {
                        if !inherited["content"].to_string().contains("LEFT") { return wiremock::ResponseTemplate::new(422); }
                        serde_json::json!({"role":"assistant","tool_calls":[{"id":"native-left-write","type":"function","function":{"name":"write","arguments":"{\"path\":\"left.txt\",\"content\":\"LEFT\\n\"}"}}]})
                    } else {
                        serde_json::json!({"role":"assistant","tool_calls":[{"id":"native-left-read","type":"function","function":{"name":"read","arguments":"{\"path\":\"left.txt\"}"}}]})
                    };
                    super::native::completion(&body, message,"tool_calls")
                }).with_priority(1).mount(&setup.backend).await;
        }
        let fixture = setup.start().await?;
        let result = async {
            let id = fixture.create().await?;
            fixture.running(&id).await?;
            let before = pending_right(&fixture, &id).await?;
            let old_left = task(&before, "left")?.executions.last().context("old left missing")?.clone();
            let original_link = old_left.thread.clone().context("original ACP binding missing")?;
            let (Observation::Candidate { run_id: Some(original_run), .. }
                | Observation::Running { run_id: Some(original_run), .. }) = &old_left.observation else {
                anyhow::bail!("original ACP run missing");
            };
            let marker = if native { "WORKER_SWITCH_NATIVE_FIXTURE" } else { "WORKER_SWITCH_ACP_FIXTURE" };
            let edited = edit_left(&fixture, &id, &format!("FIXTURE_NEW_LEFT_RULE: {marker}: replace only left's worker, retain LEFT newline and all checks." )).await?;
            let updated = task(&edited, "left")?;
            assert_eq!(updated.spec.harness.as_deref(), if native { None } else { Some("replacement-peer") });
            let original = updated.executions.iter().find(|execution| execution.id == old_left.id).context("original attempt lost")?;
            assert_eq!(updated.execution_harness(original), Some("goal-peer"));
            assert_eq!(original.thread.as_ref(), Some(&original_link));
            assert_eq!(task(&edited, "right")?.requirements_revision, 1);
            renew_independent_permission(&fixture, &id, &before).await?;
            let replaced = wait_for(&fixture.socket, &id, |goal| task(goal,"left").is_ok_and(|task| task.executions.last().is_some_and(|execution| execution.id != old_left.id && execution.thread.is_some()))).await?;
            let replacement = task(&replaced,"left")?.executions.last().context("replacement missing")?;
            let link = replacement.thread.as_ref().context("new binding missing")?;
            assert_ne!(link.thread_id, original_link.thread_id);
            assert_ne!(link.turn_id, original_link.turn_id);
            let original = task(&replaced,"left")?.executions.iter().find(|execution| execution.id == old_left.id).context("old attempt not retained")?;
            assert!(matches!(original.stop, Some(StopState::Confirmed { .. })));
            assert!(original.effects_reconciled());
            let prepared = replacement.resources.as_ref().context("replacement input missing")?;
            let handoff = prepared.worker_handoff.as_ref().context("worker transfer material missing")?;
            assert_eq!(handoff.execution_id,old_left.id);
            assert!(matches!(&original.reconciliation,Some(bitrouter_orchestrator::goal::workflow::LocalReconciliation::Verified { workspace,transcript,.. }) if workspace == &handoff.workspace && transcript == &handoff.transcript));
            let prompt = crate::goal_artifacts::read(&fixture.root(&id),&prepared.prompt).await?;
            assert!(String::from_utf8(prompt)?.contains(&serde_json::to_string(handoff)?));
            let (snapshot, _) = fixture.supervisor.managed_observation(&bitrouter_orchestrator::service::worker::acp::Owner {
                server_instance_id:fixture.threads.capabilities().server_instance_id,
                thread_id:original_link.thread_id.clone(),turn_id:original_link.turn_id.clone()
            }, original_run, 0).await?;
            assert_eq!(snapshot.agent_id, "goal-peer");
            assert!(snapshot.owned_process_terminated());
            let view = fixture.threads.read_stored_thread_view(&ThreadTarget { thread_id:link.thread_id.clone(), server_instance_id:fixture.threads.capabilities().server_instance_id }, &CallerContext::local()).await?;
            match &view.config {
                bitrouter_orchestrator::thread::executor::Config::Native(_) => assert!(native),
                bitrouter_orchestrator::thread::executor::Config::Acp { acp } => { assert!(!native); assert_eq!(acp.agent_id,"replacement-peer"); }
            }
            fixture.release().await?;
            let completed = fixture.completed(&id, 2).await?;
            let left = task(&completed,"left")?;
            assert_eq!(left.executions.len() + usize::try_from(left.archived_executions)?,2);
            let current = left.executions.last().context("current replacement missing")?;
            assert_eq!(current.thread.as_ref(),Some(link));
            assert_eq!(left.execution_harness(current),if native { None } else { Some("replacement-peer") });
            let history = fixture.left_history().await?;
            assert!(history["entries"].as_array().is_some_and(|entries| entries.iter().any(|entry| entry["execution_id"] == old_left.id && entry["harness"] == "goal-peer" && entry["run_id"] == *original_run)));
            assert_eq!(task(&completed,"right")?.executions.len(),1);
            Ok(())
        }.await;
        fixture.finish(result).await?;
    }
    Ok(())
}

fn task<'a>(goal: &'a GoalSnapshot, id: &str) -> Result<&'a Task> {
    goal.workflow
        .tasks
        .iter()
        .find(|task| task.spec.id == id)
        .context("fixture task missing")
}

async fn paused_after_workers(fixture: &Fixture, id: &str) -> Result<GoalSnapshot> {
    wait_for(&fixture.socket, id, |_| {
        fixture.controls.join("completed-left").exists()
            && fixture.controls.join("completed-right").exists()
    })
    .await?;
    control(&fixture.socket, id, "explicit-pause", RunIntent::Pause).await?;
    wait_for(&fixture.socket, id, |goal| {
        goal.run_intent == RunIntent::Pause
            && goal
                .workflow
                .tasks
                .iter()
                .filter(|task| !task.executions.is_empty())
                .all(|task| {
                    task.executions.last().is_some_and(|execution| {
                        matches!(execution.stop, Some(StopState::Confirmed { .. }))
                    })
                })
    })
    .await
}

async fn pending_right(fixture: &Fixture, id: &str) -> Result<GoalSnapshot> {
    wait_for(&fixture.socket, id, |goal| {
        fixture.controls.join("completed-left").exists()
            && goal
                .workflow
                .tasks
                .iter()
                .find(|task| task.spec.id == "right")
                .and_then(|task| task.executions.last())
                .is_some_and(|execution| {
                    let Observation::Running { run_id, .. } = &execution.observation else {
                        return false;
                    };
                    goal.workflow.decisions.iter().any(|decision| {
                        &decision.run_id == run_id
                            && decision.state
                                == bitrouter_orchestrator::goal::decision::PermissionState::Pending
                    })
                })
    })
    .await
}

async fn edit_left(fixture: &Fixture, id: &str, text: &str) -> Result<GoalSnapshot> {
    fixture
        .input(id, "edit-while-paused", text, Some("left"))
        .await?;
    let edited = wait_for(&fixture.socket, id, |goal| {
        goal.inputs
            .latest
            .as_ref()
            .is_some_and(|input| input.input_id == "edit-while-paused")
    })
    .await?;
    assert_eq!(edited.requirements_revision, 2);
    let edit = edited
        .inputs
        .latest
        .as_ref()
        .context("edit result missing")?;
    assert!(edit.requirements_saved);
    assert_eq!(edit.affected_tasks, vec!["join", "left"]);
    assert!(edit.control_saved.is_none());
    for task in &edited.workflow.tasks {
        assert_eq!(
            task.requirements_revision,
            if task.spec.id == "right" { 1 } else { 2 }
        );
    }
    assert!(!edited.workflow.completion.completed_for(2));
    assert!(!fixture.controls.join("started-join").exists());
    Ok(edited)
}

async fn replacement_receives_edit(fixture: &Fixture, id: &str, old_left: &str) -> Result<()> {
    let resumed = wait_for(&fixture.socket, id, |goal| {
        goal.workflow
            .tasks
            .iter()
            .find(|task| task.spec.id == "left")
            .and_then(|task| task.executions.last())
            .is_some_and(|execution| {
                execution.id != old_left
                    && execution.task_revision == 2
                    && matches!(execution.observation, Observation::Running { .. })
            })
    })
    .await?;
    assert_eq!(resumed.run_intent, RunIntent::Run);
    let mut adopted = false;
    let mut files = tokio::fs::read_dir(&fixture.captures).await?;
    while let Some(file) = files.next_entry().await? {
        adopted |= tokio::fs::read_to_string(file.path())
            .await?
            .contains("FIXTURE_NEW_LEFT_RULE");
    }
    assert!(
        adopted,
        "replacement harness never received the committed branch instructions"
    );
    Ok(())
}

async fn renew_independent_permission(
    fixture: &Fixture,
    id: &str,
    before: &GoalSnapshot,
) -> Result<()> {
    let original_right = &task(before, "right")?
        .executions
        .last()
        .context("right execution missing")?
        .id;
    let old = before
        .workflow
        .decisions
        .iter()
        .find(|decision| decision.execution_id == *original_right)
        .context("original pending permission missing")?
        .id
        .clone();
    assert!(
        rpc(
            &fixture.socket,
            GoalCommand::RespondDecision {
                operation_id: "stale-right-response".into(),
                goal_id: id.into(),
                decision_id: old.clone(),
                expected_requirements_revision: 2,
                option_id: "allow-once".into()
            }
        )
        .await
        .is_err()
    );
    let renewed = wait_for(&fixture.socket, id, |goal| {
        goal.workflow.decisions.iter().any(|decision| {
            decision.execution_id == *original_right
                && decision.id != old
                && decision.requirements_revision == 2
                && decision.state
                    == bitrouter_orchestrator::goal::decision::PermissionState::Pending
        })
    })
    .await?;
    let decision = renewed
        .workflow
        .decisions
        .iter()
        .find(|decision| {
            decision.execution_id == *original_right
                && decision.requirements_revision == 2
                && decision.state
                    == bitrouter_orchestrator::goal::decision::PermissionState::Pending
        })
        .context("renewed permission missing")?;
    let right = renewed
        .workflow
        .tasks
        .iter()
        .find(|task| task.spec.id == "right")
        .context("right task missing")?;
    assert_eq!(right.executions.len(), 1);
    assert_eq!(right.executions[0].id, *original_right);
    assert!(right.executions[0].stop.is_none());
    assert_eq!(right.requirements_revision, 1);
    let response = GoalCommand::RespondDecision {
        operation_id: "current-right-response".into(),
        goal_id: id.into(),
        decision_id: decision.id.clone(),
        expected_requirements_revision: 2,
        option_id: "allow-once".into(),
    };
    let mut accepted = false;
    // Background accounting may win the head CAS. Retry only a known
    // rejection with the same operation, source version and permission.
    for _ in 0..8 {
        match rpc(&fixture.socket, response.clone()).await {
            Ok(GoalReply::Accepted { operation }) => {
                assert_eq!(operation.id, "current-right-response");
                assert_eq!(operation.requirements_revision, 2);
                accepted = true;
                break;
            }
            Err(error)
                if error.downcast_ref::<bitrouter_orchestrator::goal::GoalError>()
                    == Some(&bitrouter_orchestrator::goal::GoalError::Conflict) => {}
            other => anyhow::bail!("current fixture permission was not admitted: {other:?}"),
        }
    }
    assert!(accepted, "current permission remained in conflict");
    Ok(())
}

async fn assert_archived_left_history(fixture: &Fixture) -> Result<()> {
    let history = fixture.left_history().await?;
    assert!(history["entries"].as_array().is_some_and(|entries| {
        entries.iter().any(|entry| {
            entry["committed_archive"].is_object() && entry["archive_operation_id"].is_string()
        })
    }));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paused_goal_edit_stays_paused_and_resumes_with_current_branch_versions() -> Result<()> {
    let setup = Setup::new().await?;
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        fixture.running(&id).await?;
        let before = paused_after_workers(&fixture, &id).await?;
        let old_left = task(&before, "left")?
            .executions
            .last()
            .context("left execution missing")?
            .id
            .clone();
        let edited = edit_left(&fixture, &id, LEFT_EDIT).await?;
        assert_eq!(edited.run_intent, RunIntent::Pause);
        assert_eq!(
            edited
                .workflow
                .tasks
                .iter()
                .map(|task| task.executions.len())
                .sum::<usize>(),
            2
        );
        control(&fixture.socket, &id, "explicit-resume", RunIntent::Run).await?;
        replacement_receives_edit(&fixture, &id, &old_left).await?;
        fixture.release().await?;
        fixture.completed(&id, 2).await?;

        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopped_process_without_terminal_tool_evidence_cannot_restart_or_complete() -> Result<()> {
    let mut setup = Setup::new().await?;
    setup.agent_env("BRO_GOAL_FIXTURE_OMIT_TOOL_COMPLETION");
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        fixture.running(&id).await?; paused_after_workers(&fixture, &id).await?; let edited = edit_left(&fixture, &id, LEFT_EDIT).await?;
assert_eq!(edited.run_intent, RunIntent::Pause); assert_eq!(edited.workflow.tasks.iter().map(|task| task.executions.len()).sum::<usize>(), 2);
control(&fixture.socket, &id, "explicit-resume", RunIntent::Run).await?;
                let blocked = wait_for(&fixture.socket,&id,|goal|goal.workflow.tasks.iter().filter(|task|!task.executions.is_empty()).all(|task|
                    task.executions.last().is_some_and(|execution|matches!(execution.reconciliation,
                        Some(bitrouter_orchestrator::goal::workflow::LocalReconciliation::Blocked {..}))))).await?;
                assert_eq!(blocked.run_intent,RunIntent::Run);
                assert_eq!(blocked.workflow.tasks.iter().map(|task|task.executions.len()).sum::<usize>(),2);
                assert!(!blocked.workflow.completion.completed_for(2));
                assert!(!fixture.controls.join("started-join").exists());
                assert!(blocked.workflow.tasks.iter().flat_map(|task|&task.executions).any(|execution|
                    matches!(&execution.reconciliation,Some(bitrouter_orchestrator::goal::workflow::LocalReconciliation::Blocked {reason,..})
                        if reason.contains("still unresolved"))));

        Ok(())
    }.await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_branch_edit_preserves_independent_run_and_renews_pending_permission() -> Result<()>
{
    let mut setup = Setup::new().await?;
    setup.agent_env("BRO_GOAL_FIXTURE_RIGHT_MANUAL");
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        fixture.running(&id).await?;
        let before = pending_right(&fixture, &id).await?;
        let old_left = task(&before, "left")?
            .executions
            .last()
            .context("left execution missing")?
            .id
            .clone();
        let edited = edit_left(&fixture, &id, LEFT_EDIT).await?;
        assert_eq!(edited.run_intent, RunIntent::Run);
        renew_independent_permission(&fixture, &id, &before).await?;
        replacement_receives_edit(&fixture, &id, &old_left).await?;
        fixture.release().await?;
        fixture.completed(&id, 2).await?;
        assert_archived_left_history(&fixture).await?;

        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_input_adds_machine_checks_to_the_actual_replacement_and_delivery() -> Result<()> {
    let mut setup = Setup::new().await?;
    setup.agent_env("BRO_GOAL_FIXTURE_RIGHT_MANUAL");
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        fixture.running(&id).await?;
        let before = pending_right(&fixture, &id).await?;
        let old_left = task(&before, "left")?
            .executions
            .last()
            .context("left execution missing")?
            .id
            .clone();
        let edited = edit_left(&fixture, &id, CHECK_EDIT).await?;
        assert_eq!(edited.run_intent, RunIntent::Run);
        renew_independent_permission(&fixture, &id, &before).await?;
        replacement_receives_edit(&fixture, &id, &old_left).await?;
        fixture.release().await?;
        let completed = fixture.completed(&id, 2).await?;
        let left = completed
            .workflow
            .tasks
            .iter()
            .find(|task| task.spec.id == "left")
            .context("updated checked task missing")?;
        assert_eq!(left.spec.checks.len(), 2);
        assert!(
            left.spec
                .checks
                .iter()
                .any(|check| check.id == "left-written")
        );
        let extra = left
            .spec
            .checks
            .iter()
            .find(|check| check.id == "left-keeps-seed")
            .context("additional check absent from executable spec")?;
        let execution = left
            .executions
            .last()
            .context("updated execution missing")?;
        let actual = execution
            .checks
            .iter()
            .find(|check| check.spec == *extra)
            .context("additional check was not actually dispatched")?;
        assert_eq!(actual.task_revision, 2);
        assert!(matches!(
            actual.state,
            bitrouter_orchestrator::goal::workflow::CheckState::Observed {
                passed: true,
                completed: true,
                ..
            }
        ));
        assert_archived_left_history(&fixture).await?;

        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_resource_edit_rejects_unknown_choices_and_supplies_the_new_version() -> Result<()> {
    let mut setup = Setup::new().await?;
    setup.agent_env("BRO_GOAL_FIXTURE_RIGHT_MANUAL");
    setup.routed_agent().await;
    setup.resources().await?;
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        fixture.running(&id).await?;
        let before = pending_right(&fixture, &id).await?;
        let old_left = task(&before, "left")?
            .executions
            .last()
            .context("left execution missing")?
            .id
            .clone();
        let current = head(&fixture.socket, &id).await?;
        rpc(
            &fixture.socket,
            GoalCommand::Input {
                operation_id: "invalid-resource-change".into(),
                goal_id: id.clone(),
                expected_revision: current.revision,
                text: INVALID_RESOURCE_EDIT.into(),
                task_reference: Some("left".into()),
            },
        )
        .await?;
        let rejected = wait_for(&fixture.socket, &id, |goal| {
            goal.inputs
                .latest
                .as_ref()
                .is_some_and(|input| input.input_id == "invalid-resource-change")
        })
        .await?;
        let result = rejected
            .inputs
            .latest
            .as_ref()
            .context("missing resource rejection")?;
        assert!(
            result
                .clarification
                .as_ref()
                .is_some_and(|text| text.contains("unavailable configured MCP"))
        );
        assert!(!result.requirements_saved && result.control_saved.is_none());
        assert_eq!(rejected.inputs.unresolved_inputs().len(), 1);
        assert!(rejected.inputs.completion_waits_for_input());
        assert_eq!(rejected.requirements_revision, 1);
        assert!(
            rejected
                .workflow
                .tasks
                .iter()
                .all(|task| task.spec.resources.is_empty())
        );
        assert_eq!(
            rejected
                .workflow
                .tasks
                .iter()
                .map(|task| task.executions.len())
                .sum::<usize>(),
            2
        );
        let edited = edit_left(&fixture, &id, RESOURCE_EDIT).await?;
        assert_eq!(edited.run_intent, RunIntent::Run);
        renew_independent_permission(&fixture, &id, &before).await?;
        replacement_receives_edit(&fixture, &id, &old_left).await?;
        fixture.release().await?;
        let completed = fixture.completed(&id, 2).await?;
        let root = fixture.root(&id);
        assert!(completed.inputs.unresolved_inputs().is_empty());
        assert!(completed.inputs.latest.as_ref().is_some_and(|input| {
            input
                .resolutions
                .iter()
                .any(|resolution| resolution.input_id == "invalid-resource-change")
        }));
        let completed = wait_for(&fixture.socket, &id, |goal| {
            goal.workflow
                .tasks
                .iter()
                .find(|task| task.spec.id == "left")
                .is_some_and(|task| task.archived_executions == 1)
        })
        .await?;
        let left = completed
            .workflow
            .tasks
            .iter()
            .find(|task| task.spec.id == "left")
            .context("left task missing")?;
        assert_eq!(left.executions.len(), 1);
        let pointer = left
            .latest_archive
            .as_ref()
            .context("readonly archive pointer missing")?;
        let metadata: serde_json::Value =
            serde_json::from_slice(&crate::goal_artifacts::read(&root, pointer).await?)?;
        let original_id = metadata["execution"]["id"]
            .as_str()
            .context("archive original identity missing")?;
        let archive = rpc(
            &fixture.socket,
            GoalCommand::Operation {
                operation_id: bitrouter_orchestrator::goal::workflow::archive_operation_id(
                    &id,
                    original_id,
                ),
            },
        )
        .await?;
        let GoalReply::Accepted { operation } = archive else {
            anyhow::bail!("missing cold original execution receipt")
        };
        let bitrouter_orchestrator::goal::OperationKind::Workflow { change } = operation.kind
        else {
            anyhow::bail!("unexpected archive receipt")
        };
        let Change::ArchiveExecution {
            execution: old,
            record,
            ..
        } = *change
        else {
            anyhow::bail!("missing complete original execution")
        };
        assert_eq!(&record, pointer);
        assert_eq!(
            serde_json::from_value::<Execution>(metadata["execution"].clone())?,
            *old
        );
        let inspected =
            crate::goal_code::read_archive_fixture(&fixture.socket, &root, &id, "left", pointer)
                .await?;
        assert_eq!(inspected.title, format!("Archived execution {}", old.id));
        assert!(inspected.details.iter().any(|text| text.contains(&old.id)));
        assert!(
            inspected
                .artifacts
                .iter()
                .any(|artifact| artifact.label.contains("Original tool/stop reconciliation"))
        );
        let replacement = left.executions.last().context("replacement missing")?;
        assert!(
            old.resources
                .as_ref()
                .is_some_and(|prepared| prepared.selection.is_empty())
        );
        assert!(matches!(old.stop, Some(StopState::Confirmed { .. })));
        assert_eq!(replacement.task_revision, 2);
        assert_eq!(
            replacement.confirmed_model_route.as_deref(),
            Some("goal-model")
        );
        let prepared = replacement
            .resources
            .as_ref()
            .context("new resource preparation missing")?;
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
        let mut matched = false;
        let mut files = tokio::fs::read_dir(&fixture.captures).await?;
        while let Some(file) = files.next_entry().await? {
            let text = tokio::fs::read_to_string(file.path()).await?;
            let messages = text
                .lines()
                .map(serde_json::from_str::<serde_json::Value>)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if messages.iter().any(|message| {
                message["method"] == "session/prompt"
                    && message["params"]["prompt"][0]["text"] == prompt
            }) {
                let opened = messages
                    .iter()
                    .find(|message| message["method"] == "session/new")
                    .context("new session missing")?;
                assert_eq!(
                    opened["params"]["mcpServers"].as_array().map(Vec::len),
                    Some(1)
                );
                assert_eq!(
                    opened["params"]["mcpServers"][0]["name"],
                    "selected-resource-server"
                );
                matched = true;
            }
        }
        assert!(
            matched,
            "new frozen material did not reach the replacement ACP prompt"
        );
        let right = completed
            .workflow
            .tasks
            .iter()
            .find(|task| task.spec.id == "right")
            .context("right task missing")?;
        assert_eq!(right.executions.len(), 1);
        assert_eq!(right.requirements_revision, 1);
        assert!(right.spec.resources.is_empty());

        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_review_recovers_from_goal_revision_without_worker_replay() -> Result<()> {
    let mut setup = Setup::new().await?;
    let right_reviews = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observations = right_reviews.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            if provider::model_state(request).is_some_and(|state| {
                state.get("actual_workspace").is_some() && state["task"]["id"] == "right"
            }) {
                observations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            provider::model(request)
        })
        .with_priority(1)
        .mount(&setup.backend)
        .await;
    let barrier = review_barrier::ReviewBarrier::start(setup.backend.uri()).await?;
    setup.review_api(&barrier.uri);

    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;
        let running = fixture.running(&id).await?;
        let original_right = running
            .workflow
            .tasks
            .iter()
            .find(|task| task.spec.id == "right")
            .and_then(|task| task.executions.last())
            .context("right execution missing")?
            .id
            .clone();
        tokio::fs::write(
            fixture.controls.join("release"),
            "release original fixture work",
        )
        .await?;

        tokio::time::timeout(Duration::from_secs(45), barrier.entered.notified()).await?;
        wait_for(&fixture.socket, &id, |goal| {
            right_reviews.load(std::sync::atomic::Ordering::SeqCst) > 0
                    && goal
                        .workflow
                        .tasks
                        .iter()
                        .find(|task| task.spec.id == "right")
                        .and_then(|task| task.executions.last())
                        .is_some_and(|execution| {
                            matches!(execution.review,
                        Some(bitrouter_orchestrator::goal::workflow::ReviewState::Requested { .. }))
                        })
        })
        .await?;
        fixture
            .input(
                &id,
                "edit-during-independent-review",
                REVIEW_EDIT,
                Some("left"),
            )
            .await?;
        let edited = wait_for(&fixture.socket, &id, |goal| goal.requirements_revision == 2).await?;
        let right = edited
            .workflow
            .tasks
            .iter()
            .find(|task| task.spec.id == "right")
            .context("independent task missing")?;
        assert_eq!(right.executions.len(), 1);
        assert_eq!(right.executions[0].id, original_right);
        assert_eq!(right.requirements_revision, 1);
        assert!(matches!(
            right.executions[0].review,
            Some(ReviewState::Requested {
                requirements_revision: 1,
                ..
            })
        ));
        assert!(right.executions[0].handoff.is_none());
        assert_eq!(right_reviews.load(std::sync::atomic::Ordering::SeqCst), 1);
        barrier.release.notify_one();
        fixture.release().await?;
        let completed = fixture.completed(&id, 2).await?;
        assert!(right_reviews.load(std::sync::atomic::Ordering::SeqCst) >= 2);
        let right = completed
            .workflow
            .tasks
            .iter()
            .find(|task| task.spec.id == "right")
            .context("reviewed right task missing")?;
        assert_eq!(right.executions.len(), 1);
        assert_eq!(right.requirements_revision, 1);
        let handoff = right.executions[0]
            .handoff
            .as_ref()
            .context("fresh independent handoff missing")?;
        assert_eq!(handoff.requirements_revision, 2);
        assert_eq!(handoff.task_revision, 1);
        Ok(())
    }
    .await;
    fixture.finish(result).await
}
