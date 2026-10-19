//! State fixtures and observations for the checked-in input corpus, never model labels.
use super::*;
use bitrouter_orchestrator::goal::decision::{
    PermissionDecision, PermissionOption, PermissionState,
};
use bitrouter_orchestrator::goal::input::{
    ClarificationKind, Event as InputEvent, Interpretation, Stage,
};

pub(super) async fn seed_state(
    goal: &mut GoalSnapshot,
    state: &str,
    workspaces: &Path,
) -> Result<()> {
    goal.workflow.planning = Planning::Planned {
        attempt_id: "labeled-domain-fixture".into(),
    };
    let browser = state == "browser_failure_two_branches";
    let names = if browser {
        ["safari", "chrome", "join"]
    } else {
        ["left", "right", "join"]
    };
    if browser {
        goal.objective = "Investigate Safari failing on negative parser input; reproduce and explain the failure while preserving the independent Chrome investigation.".into();
    }
    goal.workflow.tasks = names.into_iter().map(|id| {
        serde_json::from_value::<Task>(serde_json::json!({
            "spec":{"id":id,"title":id,"instruction":if id == "join" {"Combine checked branch results"} else if browser {"Investigate and reproduce this browser's parser behavior"} else {"Reproduce and verify assigned parser inputs"},
                "harness":"evaluation-only","dependencies":if id == "join" {names[..2].to_vec()} else {Vec::<&str>::new()},"checks":[]},
            "requirements_revision":1,"run_intent":"run","executions":if id == "join" {serde_json::json!([])} else {serde_json::json!([{
                "id":format!("domain-{id}"),"workspace":goal.workspace,"requirements_revision":1,"task_revision":1,"dispatch_requested":true,
                "observation":if id == "safari" {serde_json::json!({"state":"failed","run_id":"domain-safari","reason":"Labeled domain observation: negative parser input failed in Safari; the implementation cause is not known"})} else {serde_json::json!({"state":"running","run_id":format!("domain-{id}"),"native_session_id":null})}
            }])}
        }))
    }).collect::<Result<Vec<_>,_>>()?;
    if state == "paused_two_branches" {
        goal.run_intent = RunIntent::Pause;
    }
    if matches!(state, "permission_pending" | "permission_stale") {
        use agent_client_protocol::schema::v1::{
            PermissionOption as AcpOption, PermissionOptionKind, ToolCallUpdate,
            ToolCallUpdateFields, ToolKind,
        };
        let mut fields = ToolCallUpdateFields::default();
        fields.title = Some("Run the parser verification command".into());
        fields.kind = Some(ToolKind::Execute);
        fields.raw_input = Some(serde_json::json!({"command":"cargo test parser"}));
        let permission = crate::supervisor::PendingPermissionSnapshot {
            permission_id: "fixture-permission".into(),
            tool_call: ToolCallUpdate::new("fixture-parser-tool", fields),
            options: vec![
                AcpOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
                AcpOption::new(
                    "allow_always",
                    "Always allow",
                    PermissionOptionKind::AllowAlways,
                ),
            ],
        };
        let request = crate::goal_artifacts::put(
            &crate::goal_artifacts::root(workspaces, &goal.id),
            &serde_json::to_vec(&permission)?,
        )
        .await?;
        goal.workflow.decisions = vec![PermissionDecision {
            id: "d1".into(),
            execution_id: "domain-left".into(),
            run_id: "domain-left".into(),
            permission_id: "fixture-permission".into(),
            requirements_revision: 1,
            task_revision: 1,
            request,
            options: vec![
                PermissionOption {
                    id: "allow_once".into(),
                    label: "Allow once".into(),
                    allows: true,
                    persistent: false,
                },
                PermissionOption {
                    id: "allow_always".into(),
                    label: "Always allow".into(),
                    allows: true,
                    persistent: true,
                },
            ],
            state: PermissionState::Pending,
        }];
    }
    Ok(())
}

async fn current(goals: &GoalService, id: &str) -> Result<GoalSnapshot> {
    let GoalReply::Snapshot { goal } = goals
        .execute("local", GoalCommand::Get { goal_id: id.into() }, now()?)
        .await?
    else {
        anyhow::bail!("corpus snapshot missing")
    };
    Ok(*goal)
}

pub(super) async fn bootstrap_input_state(
    goals: &GoalService,
    coordinator: &str,
    id: &str,
    state: &str,
) -> Result<()> {
    if matches!(
        state,
        "open_change_question" | "halted_open_change_question" | "permission_stale"
    ) {
        let entries = if state == "halted_open_change_question" {
            vec!["u1", "u2"]
        } else {
            vec!["u1"]
        };
        for entry in entries {
            let goal = current(goals, id).await?;
            // Prior fixture input identities and times must match across model lanes.
            let accepted_at_ms = goal
                .created_at_ms
                .checked_add(1)
                .context("fixture input time overflow")?;
            let text = if state == "permission_stale" {
                "Add coverage of empty parser inputs"
            } else if entry == "u1" {
                "Also cover empty parser inputs"
            } else {
                "Also cover Unicode parser inputs"
            };
            let GoalReply::Accepted { operation } = goals
                .execute(
                    "local",
                    GoalCommand::Input {
                        operation_id: entry.into(),
                        goal_id: id.into(),
                        expected_revision: goal.revision,
                        text: text.into(),
                        task_reference: None,
                    },
                    accepted_at_ms,
                )
                .await?
            else {
                anyhow::bail!("original fixture input receipt missing")
            };
            let attempt = format!("domain-fixture-{entry}");
            goals
                .workflow_change(
                    WorkflowActor {
                        owner: "local",
                        coordinator_id: coordinator,
                    },
                    id,
                    &format!("seed-begin-{entry}"),
                    goal.requirements_revision,
                    Change::Input {
                        event: InputEvent::Begin {
                            operation: Box::new(operation),
                            attempt_id: attempt.clone(),
                            stage: Stage::Reasoning,
                        },
                    },
                    now()?,
                )
                .await?;
            let interpretation = if state == "permission_stale" {
                Interpretation {
                    objective: Some(format!(
                        "{} Also cover empty parser inputs.",
                        goal.objective
                    )),
                    task_updates: goal
                        .workflow
                        .tasks
                        .iter()
                        .map(|task| bitrouter_orchestrator::goal::input::TaskUpdate {
                            task_id: task.spec.id.clone(),
                            instruction: format!(
                                "{} Also cover empty parser inputs.",
                                task.spec.instruction
                            ),
                            additional_checks: Vec::new(),
                            resources: None,
                        })
                        .collect(),
                    ..Default::default()
                }
            } else {
                Interpretation {
                    clarification: Some(format!(
                        "Labeled fixture question for {entry}: should the new input category be accepted or rejected?"
                    )),
                    clarification_kind: Some(ClarificationKind::Change),
                    ..Default::default()
                }
            };
            // This prior conversation is an explicit seed fixture. No provider
            // was called; the next message is the actual evaluated request.
            goals
                .workflow_change(
                    WorkflowActor {
                        owner: "local",
                        coordinator_id: coordinator,
                    },
                    id,
                    &format!("seed-finish-{entry}"),
                    goal.requirements_revision,
                    Change::Input {
                        event: InputEvent::Finish {
                            attempt_id: attempt,
                            interpretation,
                            reasoning_request_id: Some("domain-fixture-not-a-model-request".into()),
                        },
                    },
                    now()?,
                )
                .await?;
        }
    }
    if state == "halted_open_change_question" {
        let goal = current(goals, id).await?;
        goals
            .workflow_change(
                WorkflowActor {
                    owner: "local",
                    coordinator_id: coordinator,
                },
                id,
                "seed-budget-halt",
                goal.requirements_revision,
                Change::Budget {
                    event: bitrouter_orchestrator::goal::budget::Event::Halt {
                        reason: "Labeled fixture: managed budget admission halted".into(),
                    },
                },
                now()?,
            )
            .await?;
    }
    Ok(())
}

pub(super) async fn model_evidence(
    coordinator: &Coordinator,
    id: &str,
    operations: &[GoalOperation],
) -> Result<Vec<serde_json::Value>> {
    let root = crate::goal_artifacts::root(&coordinator.workspaces, id);
    let mut records = Vec::new();
    for operation in operations {
        if let OperationKind::Workflow { change } = &operation.kind
            && let Change::Accounting {
                event: bitrouter_orchestrator::goal::accounting::Event::Begin { work },
            } = change.as_ref()
        {
            let value: serde_json::Value =
                serde_json::from_slice(&crate::goal_artifacts::read(&root, &work.intent).await?)?;
            if value.get("prompt").is_some() && value.get("routes").is_some() {
                records
                    .push(serde_json::json!({"work_id":work.id,"intent":work.intent,"plan":value}));
            }
        }
        if let OperationKind::Workflow { change } = &operation.kind
            && let Change::Accounting {
                event: bitrouter_orchestrator::goal::accounting::Event::Observe { id, report, .. },
            } = change.as_ref()
        {
            let value: serde_json::Value =
                serde_json::from_slice(&crate::goal_artifacts::read(&root, report).await?)?;
            if value.get("result").is_some() || value.get("accepted_routes").is_some() {
                records.push(serde_json::json!({"work_id":id,"report":report,"observation":value}));
            }
        }
    }
    Ok(records)
}

pub(super) fn effect_violations(
    case: &serde_json::Value,
    before: &GoalSnapshot,
    after: &GoalSnapshot,
    feedback: &[String],
) -> Result<Vec<String>> {
    let expected = &case["expected"];
    let result = after.inputs.latest.as_ref();
    let mut violations = Vec::new();
    if let Some(wanted) = expected.get("clarification_kind") {
        let actual = serde_json::to_value(result.map(|result| result.clarification_kind))?;
        if &actual != wanted {
            violations.push("clarification kind differs from annotation".into());
        }
    }
    let control = serde_json::to_value(result.and_then(|result| result.control_saved.as_ref()))?;
    let permission =
        serde_json::to_value(result.and_then(|result| result.permission_response_saved.as_ref()))?;
    let requirement_change = after.requirements_revision != before.requirements_revision;
    if let Some(wanted) = expected.get("modification_scope") {
        let actual = serde_json::to_value(
            result
                .and_then(|result| result.modification_scope.as_ref())
                .map(|binding| &binding.selection),
        )?;
        if &actual != wanted {
            violations.push("recorded modification boundary differs from annotation".into());
        }
    }
    if expected["query_clarification_may_coexist"] == true
        && result.is_some_and(|result| result.clarification.is_some())
        && (result.is_none_or(|result| result.clarification_kind != ClarificationKind::Query)
            || after.inputs.completion_waits_for_input())
    {
        violations
            .push("missing query evidence became a change ambiguity or completion blocker".into());
    }
    let root_intent = if expected["control"]["target"] == "goal" {
        serde_json::from_value(expected["control"]["intent"].clone())?
    } else {
        before.run_intent
    };
    if !requirement_change {
        for task in &before.workflow.tasks {
            if after
                .workflow
                .tasks
                .iter()
                .find(|after| after.spec.id == task.spec.id)
                .is_none_or(|after| {
                    after.spec != task.spec
                        || after.requirements_revision != task.requirements_revision
                })
            {
                violations.push(format!(
                    "task {} requirements changed without a new Goal version",
                    task.spec.id
                ));
            }
        }
    }
    let meaningful_feedback = feedback.iter().any(|line| {
        [
            "BRO model answer:",
            "Needs clarification / unavailable:",
            "Control intent saved:",
            "Requirements saved",
            "Permission response saved:",
            "Input ",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
    });
    for (passed, label) in [
        (
            control == expected["control"],
            "saved control differs from annotation",
        ),
        (
            permission == expected["permission"],
            "saved permission differs from annotation",
        ),
        (
            requirement_change == expected["requirements_change"].as_bool().unwrap_or(false),
            "requirements change differs from annotation",
        ),
        (
            after.run_intent == root_intent,
            "Goal run intent differs from requested scope",
        ),
        (
            !expected["answer"].as_bool().unwrap_or(false) || meaningful_feedback,
            "no actual user-facing input feedback",
        ),
        (
            expected["clarification"].as_bool().is_none_or(|wanted| {
                result.is_some_and(|result| result.clarification.is_some()) == wanted
            }),
            "clarification differs from annotation",
        ),
    ] {
        if !passed {
            violations.push(label.into());
        }
    }
    if let Some(checks) = expected["additional_checks"].as_object() {
        for (task_id, required) in checks {
            let before_task = before
                .workflow
                .tasks
                .iter()
                .find(|task| &task.spec.id == task_id);
            let after_task = after
                .workflow
                .tasks
                .iter()
                .find(|task| &task.spec.id == task_id);
            let preserved = before_task.zip(after_task).is_some_and(|(before, after)| {
                before
                    .spec
                    .checks
                    .iter()
                    .all(|check| after.spec.checks.contains(check))
            });
            let actual = after_task.map(|task| &task.spec.checks);
            let wanted = required
                .as_array()
                .context("additional check annotation must be an array")?;
            let matched = wanted.iter().all(|wanted| {
                actual.is_some_and(|checks| {
                    checks.iter().any(|check| {
                        Some(check.id.as_str()) == wanted["id"].as_str()
                            && serde_json::to_value(&check.recipe).ok().as_ref()
                                == Some(&wanted["recipe"])
                    })
                })
            });
            if !preserved || !matched {
                violations.push(format!(
                    "requested executable checks not saved or original checks changed: {task_id}"
                ));
            }
        }
    }
    if let Some(affected) = expected["affected_tasks"].as_array() {
        let wanted = affected
            .iter()
            .filter_map(|id| id.as_str())
            .collect::<BTreeSet<_>>();
        let actual = result
            .map(|result| {
                result
                    .affected_tasks
                    .iter()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        if wanted != actual {
            violations.push("affected task closure differs from annotation".into());
        }
        for task in &before.workflow.tasks {
            if !wanted.contains(task.spec.id.as_str())
                && after
                    .workflow
                    .tasks
                    .iter()
                    .find(|after| after.spec.id == task.spec.id)
                    != Some(task)
            {
                violations.push(format!("independent task {} changed", task.spec.id));
            }
        }
    }
    if expected["control"]["target"]
        .as_str()
        .is_some_and(|target| target.starts_with("task:"))
    {
        let target = expected["control"]["target"]
            .as_str()
            .context("control target missing")?
            .trim_start_matches("task:");
        let controlled = result
            .map(|result| {
                result
                    .controlled_tasks
                    .iter()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        if !controlled.contains(target) || !controlled.contains("join") {
            violations.push("task control did not include its dependency closure".into());
        }
        let intent: RunIntent = serde_json::from_value(expected["control"]["intent"].clone())?;
        for task in &after.workflow.tasks {
            if controlled.contains(task.spec.id.as_str()) && task.run_intent != intent {
                violations.push(format!(
                    "controlled task {} run intent did not match the saved control",
                    task.spec.id
                ));
            }
        }
        for task in &before.workflow.tasks {
            if !controlled.contains(task.spec.id.as_str())
                && after
                    .workflow
                    .tasks
                    .iter()
                    .find(|after| after.spec.id == task.spec.id)
                    != Some(task)
            {
                violations.push(format!(
                    "independent task {} changed during control",
                    task.spec.id
                ));
            }
        }
    }
    if expected["control"].is_null() {
        for task in &before.workflow.tasks {
            if after
                .workflow
                .tasks
                .iter()
                .find(|after| after.spec.id == task.spec.id)
                .is_none_or(|after| after.run_intent != task.run_intent)
            {
                violations.push(format!(
                    "task {} control changed without a requested control",
                    task.spec.id
                ));
            }
        }
    }
    let unresolved = after
        .inputs
        .unresolved_inputs()
        .into_iter()
        .map(|input| input.input_id)
        .collect::<BTreeSet<_>>();
    for (field, present) in [("retained_unresolved", true), ("removed_unresolved", false)] {
        if let Some(ids) = expected[field].as_array() {
            for id in ids.iter().filter_map(|id| id.as_str()) {
                if unresolved.contains(id) != present {
                    violations.push(format!("unresolved input {id} has wrong disposition"));
                }
            }
        }
    }
    if expected["route"] == "deterministic" {
        if before.accounting != after.accounting || after.budget != before.budget {
            violations.push("deterministic withdrawal changed accounting or budget".into());
        }
        for input in before.inputs.unresolved_inputs() {
            if input.input_id != "u1" && !unresolved.contains(&input.input_id) {
                violations.push("withdrawal cleared a different input".into());
            }
        }
    }
    Ok(violations)
}

#[tokio::test]
async fn corpus_seeds_keep_actual_original_input_receipts_without_model_work() -> Result<()> {
    for state in [
        "running_two_branches",
        "paused_two_branches",
        "permission_pending",
        "permission_stale",
        "open_change_question",
        "halted_open_change_question",
        "browser_failure_two_branches",
    ] {
        let root = tempfile::tempdir()?;
        let source_db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&source_db).await?;
        let source = Arc::new(crate::goal_store::DatabaseGoalStore::new(source_db));
        let service = GoalService::new(source.clone());
        let GoalReply::Accepted { operation } = service
            .execute(
                "local",
                GoalCommand::Create {
                    operation_id: "seed".into(),
                    title: "Domain fixture".into(),
                    workspace: root.path().into(),
                    objective: "Check parser inputs".into(),
                    budget_limits: Default::default(),
                },
                now()?,
            )
            .await?
        else {
            anyhow::bail!("seed receipt absent")
        };
        let mut seed = source.get("local", &operation.goal_id).await?;
        seed_state(&mut seed, state, root.path()).await?;
        if matches!(state, "permission_pending" | "permission_stale") {
            let decision = &seed.workflow.decisions[0];
            let material = crate::goal_artifacts::read(
                &crate::goal_artifacts::root(root.path(), &seed.id),
                &decision.request,
            )
            .await?;
            let permission: crate::supervisor::PendingPermissionSnapshot =
                serde_json::from_slice(&material)?;
            assert_eq!(permission.permission_id, decision.permission_id);
            assert_eq!(permission.options.len(), decision.options.len());
        }
        let target_db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&target_db).await?;
        let target = Arc::new(crate::goal_store::DatabaseGoalStore::new(target_db));
        target.commit(None, seed.clone(), operation).await?;
        let service = GoalService::new(target.clone());
        service
            .workflow_change(
                WorkflowActor {
                    owner: "local",
                    coordinator_id: "fixture",
                },
                &seed.id,
                "claim",
                1,
                Change::ClaimCoordinator {
                    id: "fixture".into(),
                    expires_at_ms: now()? + OWNER_LEASE_MS,
                },
                now()?,
            )
            .await?;
        bootstrap_input_state(&service, "fixture", &seed.id, state).await?;
        let after = current(&service, &seed.id).await?;
        assert_eq!(after.accounting.work_started, 0);
        assert_eq!(after.accounting.provider_attempts, 0);
        if matches!(
            state,
            "open_change_question" | "halted_open_change_question"
        ) {
            let context = service
                .clarification_context("local", &seed.id, None)
                .await?;
            assert_eq!(
                context.len(),
                if state == "halted_open_change_question" {
                    2
                } else {
                    1
                }
            );
            let original = target
                .operation("local", "u1")
                .await?
                .context("original fixture receipt absent")?;
            assert_eq!(original.accepted_at_ms, seed.created_at_ms + 1);
        }
        if state == "permission_stale" {
            assert_eq!(after.requirements_revision, 2);
            assert_eq!(after.workflow.decisions[0].requirements_revision, 1);
            assert!(
                service
                    .execute(
                        "local",
                        GoalCommand::RespondDecision {
                            operation_id: "stale-fixture-reply".into(),
                            goal_id: seed.id.clone(),
                            decision_id: "d1".into(),
                            expected_requirements_revision: 1,
                            option_id: "allow_once".into()
                        },
                        now()?
                    )
                    .await
                    .is_err()
            );
            assert_eq!(current(&service, &seed.id).await?, after);
        }
        if state == "halted_open_change_question" {
            assert!(after.budget.resume_required);
        }
    }
    Ok(())
}
