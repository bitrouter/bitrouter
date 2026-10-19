use super::*;
use bitrouter_orchestrator::goal::completion::State as CompletionState;
use bitrouter_orchestrator::goal::decision::PermissionState;
use bitrouter_orchestrator::goal::workflow::{
    LocalReconciliation, Observation, Planning, ReviewState, StopState,
};
use bitrouter_tui::goals::{Acceptance, Artifact, Decision, DecisionOption, Facts, Goal, Task};

pub(super) type Artifacts =
    BTreeMap<(String, String), bitrouter_orchestrator::goal::workflow::ArtifactRef>;

fn artifact(
    goal: &str,
    label: &str,
    reference: &bitrouter_orchestrator::goal::workflow::ArtifactRef,
    map: &mut Artifacts,
) -> Artifact {
    map.insert((goal.into(), reference.sha256.clone()), reference.clone());
    Artifact {
        id: reference.sha256.clone(),
        label: format!(
            "{label} · SHA {} · {} bytes",
            reference.sha256, reference.bytes
        ),
    }
}

pub(super) fn goal(snapshot: &GoalSnapshot, map: &mut Artifacts) -> Goal {
    let progress = &snapshot.workflow.completion;
    let complete = progress.completed_for(snapshot.requirements_revision)
        && !snapshot.inputs.completion_waits_for_input();
    let mut facts = Facts {
        completed: complete,
        paused: snapshot.run_intent == RunIntent::Pause,
        budget_limited: snapshot.budget.resume_required || snapshot.budget.halt.is_some(),
        running: !snapshot.accounting.in_flight.is_empty(),
        blocked: (snapshot.workflow.planning_requirements_revision
            == snapshot.requirements_revision
            && matches!(snapshot.workflow.planning, Planning::Blocked { .. }))
            || progress.attempt.as_ref().is_some_and(|attempt| {
                attempt.requirements_revision == snapshot.requirements_revision
                    && matches!(attempt.state, CompletionState::Blocked { .. })
            }),
        has_results: progress.last_completed.is_some(),
    };
    let mut reason = snapshot
        .budget
        .halt
        .clone()
        .unwrap_or_else(|| match snapshot.run_intent {
            RunIntent::Pause => "Paused by you".into(),
            RunIntent::Stop => "Stop requested by you".into(),
            RunIntent::Run => match &snapshot.workflow.planning {
                Planning::Blocked { reason, .. } => format!("Planning blocked: {reason}"),
                _ => "Continuing within current requirements".into(),
            },
        });
    let mut execution = if snapshot.workflow.tasks.is_empty() {
        "No worker execution recorded"
    } else {
        "Settled / waiting dependencies"
    }
    .to_owned();
    let mut tasks = Vec::new();
    let mut recovery_needs_you = false;
    for task in &snapshot.workflow.tasks {
        let mut detail = vec![
            format!(
                "Task requirements revision: {} · Goal requirements: {}",
                task.requirements_revision, snapshot.requirements_revision
            ),
            task.spec.instruction.clone(),
        ];
        let mut artifacts = Vec::new();
        if task.archived_executions != 0 {
            detail.push(format!("{} older reconciled executions retained in immutable Goal history; current attempts and budget are unchanged", task.archived_executions));
        }
        if let Some(record) = &task.latest_archive {
            artifacts.push(artifact(
                &snapshot.id,
                "Older execution record · read only",
                record,
                map,
            ));
        }
        detail.push(format!("Requested resources · task revision {} · BRO route {} · MCP {:?} · context {:?} · skills {:?}",
            task.requirements_revision, task.spec.resources.model_route.as_deref().unwrap_or("harness default / uncontrolled"),
            task.spec.resources.mcp_servers, task.spec.resources.context_files,
            task.spec.resources.skills.iter().map(|skill| (&skill.material_id, &skill.sha256)).collect::<Vec<_>>()));
        let status = if let Some(attempt) = task.executions.last() {
            detail.push(format!(
                "Execution: {} · source Goal requirements {} · task revision {}",
                attempt.id, attempt.requirements_revision, attempt.task_revision
            ));
            detail.push(format!("Workspace: {}", attempt.workspace.display()));
            detail.push(format!(
                "Dispatch admitted: {} · stop fact: {:?}",
                attempt.dispatch_requested, attempt.stop
            ));
            if matches!(attempt.stop, Some(StopState::Unknown { .. })) {
                execution = "Effects unknown".into();
                facts.blocked = true;
            }
            facts.blocked |= matches!(attempt.observation, Observation::Unknown { .. })
                && !attempt.effects_reconciled()
                || attempt.task_revision == task.requirements_revision
                    && (matches!(attempt.observation, Observation::Failed { .. })
                        || matches!(attempt.review, Some(ReviewState::Rejected { .. })));
            facts.running |= matches!(attempt.observation, Observation::Running { .. })
                && !matches!(attempt.stop, Some(StopState::Confirmed { .. }));
            facts.has_results |= matches!(attempt.observation, Observation::Candidate { .. });
            for check in &attempt.checks {
                detail.push(format!(
                    "Check {} · artifact {} · {:?}",
                    check.spec.id, check.workspace.sha256, check.state
                ));
                if let bitrouter_orchestrator::goal::workflow::CheckState::Observed {
                    report, ..
                } = &check.state
                {
                    artifacts.push(artifact(
                        &snapshot.id,
                        &format!("Check {}", check.spec.id),
                        report,
                        map,
                    ));
                }
            }
            if let Some(handoff) = &attempt.handoff {
                artifacts.push(artifact(
                    &snapshot.id,
                    "Checked workspace",
                    &handoff.workspace,
                    map,
                ));
                artifacts.push(artifact(
                    &snapshot.id,
                    "Semantic handoff review",
                    &handoff.review,
                    map,
                ));
            }
            if let Some(ReviewState::Rejected { reason, report, .. }) = &attempt.review {
                detail.push(format!("Review blocked: {reason}"));
                if let Some(report) = report {
                    artifacts.push(artifact(&snapshot.id, "Review evidence", report, map));
                }
            }
            match &attempt.observation {
                Observation::Unknown { .. }
                    if attempt.replacement_ready(
                        task.requirements_revision,
                        snapshot
                            .control_source_revision
                            .max(task.control_source_revision),
                    ) =>
                {
                    "local work reconciled / replacement pending"
                }
                Observation::Unknown { reason, .. } => {
                    execution = "Effects unknown".into();
                    detail.push(reason.clone());
                    "effects unknown"
                }
                Observation::Running { run_id, .. } => {
                    detail.push(format!("Supervisor run: {run_id}"));
                    if matches!(attempt.stop, Some(StopState::Confirmed { .. })) {
                        "owned process stopped"
                    } else {
                        if execution != "Effects unknown" {
                            execution = if attempt.stop.is_some() {
                                "Stopping (request/fact differ)"
                            } else {
                                "Running (last committed observation)"
                            }
                            .into();
                        }
                        "last observed running"
                    }
                }
                Observation::Requested => {
                    if execution != "Effects unknown" {
                        execution = "Preparing / startup unresolved".into();
                    }
                    "preparing"
                }
                Observation::Candidate { report, .. } => {
                    artifacts.insert(
                        0,
                        artifact(
                            &snapshot.id,
                            "Worker candidate · unverified by itself",
                            report,
                            map,
                        ),
                    );
                    if attempt
                        .handoff
                        .as_ref()
                        .is_some_and(|handoff| handoff.task_revision == task.requirements_revision)
                    {
                        "handoff verified for task version"
                    } else if matches!(attempt.stop, Some(StopState::Confirmed { .. })) {
                        "candidate / checks or review pending"
                    } else {
                        if execution != "Effects unknown" {
                            execution = "Stopping candidate process".into();
                        }
                        "candidate / stop unconfirmed"
                    }
                }
                Observation::Failed { reason, .. } => {
                    detail.push(reason.clone());
                    "failed / incomplete"
                }
            }
        } else {
            "waiting dependencies / not dispatched"
        };
        for attempt in &task.executions {
            match &attempt.reconciliation {
                Some(LocalReconciliation::Verified {
                    task_revision,
                    workspace,
                    transcript,
                    ..
                }) => {
                    detail.push(format!("Prior execution {}: finished scoped local tools and stopped workspace reconciled for task version {task_revision}. Replacement dispatch and verification remain separate.",attempt.id));
                    artifacts.push(artifact(
                        &snapshot.id,
                        "Stopped local workspace",
                        workspace,
                        map,
                    ));
                    artifacts.push(artifact(
                        &snapshot.id,
                        "Actual tool/stop reconciliation evidence",
                        transcript,
                        map,
                    ));
                }
                Some(LocalReconciliation::Blocked {
                    task_revision,
                    control_source_revision,
                    reason,
                }) => {
                    detail.push(format!(
                        "Replacement evidence for task version {task_revision}: {reason}"
                    ));
                    if *task_revision == task.requirements_revision
                        && *control_source_revision
                            == snapshot
                                .control_source_revision
                                .max(task.control_source_revision)
                        && task.executions.last() == Some(attempt)
                    {
                        facts.blocked = true;
                        recovery_needs_you = true;
                    }
                }
                None => {}
            }
        }
        for decision in &snapshot.workflow.decisions {
            if task
                .executions
                .iter()
                .any(|execution| execution.id == decision.execution_id)
            {
                artifacts.push(artifact(
                    &snapshot.id,
                    "Actual permission request",
                    &decision.request,
                    map,
                ));
                detail.push(format!(
                    "Permission {} · source requirements {} / task {} · {:?}",
                    decision.id,
                    decision.requirements_revision,
                    decision.task_revision,
                    decision.state
                ));
            }
        }
        if let Some(prepared) = task
            .executions
            .last()
            .and_then(|execution| execution.resources.as_ref())
        {
            detail.push(format!("Selected BRO model route: {} · controller confirmed: {} · actual provider model identity: unreported",
                prepared.selection.model_route.as_deref().unwrap_or("harness default / uncontrolled"),
                task.executions.last().and_then(|execution| execution.confirmed_model_route.as_deref()).unwrap_or("unreported")));
            detail.push(format!("Selected MCP servers: {:?}; descriptor configuration prepared, actual tool use unreported", prepared.selection.mcp_servers));
            detail.push("Context and skill instruction bytes provided by BRO; private harness activation/model identity remain unreported unless separately evidenced".into());
            artifacts.push(artifact(
                &snapshot.id,
                "Prepared execution input",
                &prepared.prompt,
                map,
            ));
            for material in &prepared.materials {
                use bitrouter_orchestrator::goal::resources::Material;
                let (label, content) = match material {
                    Material::Context { path, content } => {
                        (format!("Provided context / {path}"), content)
                    }
                    Material::Skill { id, name, content } => {
                        (format!("Provided skill / {name} / {id}"), content)
                    }
                };
                artifacts.push(artifact(&snapshot.id, &label, content, map));
            }
        }
        tasks.push(Task {
            id: task.spec.id.clone(),
            title: task.spec.title.clone(),
            status: status.into(),
            dependencies: task.spec.dependencies.clone(),
            resource: format!(
                "ACP harness: {} · BRO route: {} · MCP selected: {} · skills selected: {} · context selected: {} · actual provider/use: unreported",
                task.spec.harness, task.spec.resources.model_route.as_deref().unwrap_or("harness default"), task.spec.resources.mcp_servers.len(), task.spec.resources.skills.len(), task.spec.resources.context_files.len()
            ),
            details: detail,
            artifacts,
        });
    }
    let decisions: Vec<_> = snapshot
        .workflow
        .decisions
        .iter()
        .filter(|decision| decision.state == PermissionState::Pending)
        .map(|decision| {
            let task = snapshot.workflow.tasks.iter().find(|task| {
                task.executions
                    .iter()
                    .any(|execution| execution.id == decision.execution_id)
            });
            let execution = task.and_then(|task| {
                task.executions
                    .iter()
                    .find(|execution| execution.id == decision.execution_id)
            });
            Decision {
                id: decision.id.clone(),
                requirements_revision: decision.requirements_revision,
                label: format!(
                    "Permission for task {} · harness {}",
                    task.map_or("unreported", |task| task.spec.id.as_str()),
                    task.map_or("unreported", |task| task.spec.harness.as_str())
                ),
                details: vec![
                    format!(
                        "Execution {} / supervisor run {}",
                        decision.execution_id, decision.run_id
                    ),
                    format!(
                        "Request requirements {} / task {}; current Goal requirements {}",
                        decision.requirements_revision,
                        decision.task_revision,
                        snapshot.requirements_revision
                    ),
                    format!(
                        "Assigned workspace: {}",
                        execution
                            .map(|execution| execution.workspace.display().to_string())
                            .unwrap_or_else(|| "unreported".into())
                    ),
                    format!("Goal reply: /decision {} <your message>", decision.id),
                    "Persistent allow: choose the actual card option, or explicitly name /option <offered-id> in that reply".into(),
                ],
                request: artifact(
                    &snapshot.id,
                    "Actual permission request",
                    &decision.request,
                    map,
                ),
                options: decision
                    .options
                    .iter()
                    .map(|option| DecisionOption {
                        id: option.id.clone(),
                        label: option.label.clone(),
                        allows: option.allows,
                        persistent: option.persistent,
                    })
                    .collect(),
            }
        })
        .collect();
    let mut result_artifacts = Vec::new();
    let mut delivery = "None".to_owned();
    if let Some(record) = &progress.last_completed {
        delivery = format!(
            "Verified delivery / requirements {}{}",
            record.requirements_revision,
            if complete {
                " (current)"
            } else {
                " (previous version)"
            }
        );
        result_artifacts.push(artifact(
            &snapshot.id,
            "Completed delivery",
            &record.delivery,
            map,
        ));
        result_artifacts.push(artifact(
            &snapshot.id,
            "Completion assessment",
            &record.assessment.report,
            map,
        ));
    }
    let mut acceptance = None;
    if let Some(attempt) = &progress.attempt {
        if let Some(reference) = &attempt.delivery {
            result_artifacts.push(artifact(
                &snapshot.id,
                "Integrated delivery candidate",
                reference,
                map,
            ));
            if !complete {
                delivery = format!(
                    "Integrated candidate / requirements {} / {:?}",
                    attempt.requirements_revision, attempt.state
                );
            }
            if attempt.requirements_revision == snapshot.requirements_revision
                && attempt.state == CompletionState::AwaitingAcceptance
            {
                acceptance = Some(Acceptance {
                    attempt_id: attempt.id.clone(),
                    requirements_revision: attempt.requirements_revision,
                    delivery_sha256: reference.sha256.clone(),
                });
            }
        }
        if let Some(assessment) = &attempt.assessment {
            result_artifacts.push(artifact(
                &snapshot.id,
                "Candidate assessment",
                &assessment.report,
                map,
            ));
        }
    }
    let conditions = progress
        .specification
        .as_ref()
        .map(|spec| {
            spec.conditions
                .iter()
                .map(|condition| {
                    let status = progress
                        .attempt
                        .as_ref()
                        .filter(|attempt| {
                            attempt.requirements_revision == snapshot.requirements_revision
                        })
                        .and_then(|attempt| attempt.assessment.as_ref())
                        .and_then(|assessment| {
                            assessment
                                .verdict
                                .conditions
                                .iter()
                                .find(|result| result.id == condition.id)
                        })
                        .map(|result| format!("{:?}: {}", result.status, result.reason))
                        .unwrap_or_else(|| "Unverified".into());
                    format!("{}: {} · {}", condition.id, condition.description, status)
                })
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                "Completion conditions: not yet planned; original objective remains required"
                    .into(),
            ]
        });
    facts.has_results |= !result_artifacts.is_empty();
    if recovery_needs_you && snapshot.run_intent == RunIntent::Run && snapshot.budget.halt.is_none()
    {
        reason = "A stopped branch needs tool/workspace reconciliation; inspect its recorded recovery reason and evidence".into();
    }
    if snapshot.inputs.completion_waits_for_input()
        && snapshot.run_intent == RunIntent::Run
        && snapshot.budget.halt.is_none()
    {
        reason = "Completion waits for unresolved input; independent authorized work can continue"
            .into();
    }
    Goal {
        id: snapshot.id.clone(),
        revision: snapshot.revision,
        title: snapshot.title.clone(),
        workspace: snapshot.workspace.display().to_string(),
        objective: snapshot.objective.clone(),
        satisfaction: if complete { "complete" } else { "incomplete" }.into(),
        execution,
        reason,
        needs_you: !decisions.is_empty()
            || recovery_needs_you
            || acceptance.is_some()
            || !snapshot.inputs.unresolved_inputs().is_empty(),
        delivery,
        freshness: format!(
            "Goal snapshot revision {} · last durable commit {} ms UTC; execution facts may be older",
            snapshot.revision, snapshot.updated_at_ms
        ),
        conditions,
        tasks,
        decisions,
        acceptance,
        artifacts: result_artifacts,
        facts,
        budget_limits: bitrouter_tui::goals::budget::Limits {
            elapsed_ms: snapshot.budget.limits.elapsed_ms,
            managed_token_estimates_micro_usd: snapshot
                .budget
                .limits
                .managed_token_estimates_micro_usd,
        },
        budget: vec![
            format!(
                "Known model-token estimates: {} micro-USD · held bound: {}",
                snapshot.accounting.estimated_model_tokens_micro_usd,
                snapshot
                    .budget
                    .reserved_micro_usd()
                    .map(|amount| amount.to_string())
                    .unwrap_or_else(|_| "unknown".into())
            ),
            format!(
                "Budget limits: {:?} · resume required: {}",
                snapshot.budget.limits, snapshot.budget.resume_required
            ),
            format!(
                "Managed generation attempts: {} · work observations without token estimates: {} · external harness/tools/bills may be unknown",
                snapshot.accounting.provider_attempts, snapshot.accounting.unpriced_outcomes
            ),
        ],
    }
}

pub(super) fn permission_text(bytes: &[u8]) -> Result<String> {
    let permission: crate::supervisor::PendingPermissionSnapshot = serde_json::from_slice(bytes)
        .context("permission evidence is not a supported actual request")?;
    let tool = serde_json::to_value(&permission.tool_call)?;
    Ok(format!(
        "Requested operation (harness reported): {}\nKind (harness reported): {}\nDeclared locations: {}\n\nActual request:\n{}",
        tool.get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unreported"),
        tool.get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unreported"),
        tool.get("locations")
            .map(serde_json::Value::to_string)
            .unwrap_or_else(|| "unreported".into()),
        serde_json::to_string_pretty(&permission)?
    ))
}

pub(super) fn operation(operation: &GoalOperation) -> Option<String> {
    use bitrouter_orchestrator::goal::OperationKind;
    use bitrouter_orchestrator::goal::workflow::Change;
    let detail = match &operation.kind {
        OperationKind::Created => "Goal accepted; not a claim of execution".into(),
        OperationKind::Input {
            text,
            task_reference,
        } => format!(
            "Input received / scope {}: {}",
            task_reference.as_deref().unwrap_or("Goal"),
            text
        ),
        OperationKind::Control { intent } => {
            format!("Goal control intent saved: {intent:?}; actual execution is separate")
        }
        OperationKind::DecisionResponse {
            decision_id,
            option_id,
        } => format!(
            "Permission response saved: {decision_id} / {option_id}; transmission/effects are separate"
        ),
        OperationKind::DeliveryAccepted {
            delivery_sha256, ..
        } => {
            format!("Human accepted delivery {delivery_sha256}; completion submission is separate")
        }
        OperationKind::BudgetUpdated { limits } => {
            format!("Budget limits saved: {limits:?}; cumulative consumption retained")
        }
        OperationKind::InputWithdrawn { input_id, source_requirements_revision } => format!(
            "Your explicit withdrawal of input {input_id} / source requirements {source_requirements_revision} was saved without model use; existing requirements, controls, effects and budget remain"
        ),
        OperationKind::Workflow { change } => match change.as_ref() {
            Change::ArchiveExecution { task_id, execution, .. } => format!(
                "Execution {} / task {} / source Goal requirements {} / task revision {} archived with its complete original record. Work and effects were not replayed.",
                execution.id, task_id, execution.requirements_revision, execution.task_revision
            ),
            Change::Input {
                event: bitrouter_orchestrator::goal::input::Event::Finish { .. },
            } => "Input processing recorded; effective changes are reported from the runtime result, not the model proposal".into(),
            Change::Accounting { .. } | Change::ClaimCoordinator { .. } => return None,
            other => format!("Committed workflow fact: {other:?}"),
        },
    };
    Some(format!(
        "revision {} / requirements {} / {}: {}",
        operation.revision,
        operation.requirements_revision,
        operation.id,
        detail.chars().take(6000).collect::<String>()
    ))
}

pub(super) fn input_lines(goal: &GoalSnapshot) -> Vec<String> {
    let mut lines = goal.inputs.unresolved_inputs().iter().map(|input| format!(
        "Unresolved input {} · source requirements {} · {:?} · {} · /input {} references this question (/withdraw cancels the uncommitted request): {}",
        input.input_id, input.source_requirements_revision, input.kind,
        if input.kind == bitrouter_orchestrator::goal::input::ClarificationKind::Query { "does not block completion" } else { "completion waits; independent work continues" },
        input.input_id, input.question.chars().take(2048).collect::<String>()
    )).collect::<Vec<_>>();
    let Some(input) = &goal.inputs.latest else {
        return lines;
    };
    lines.push(format!(
        "Runtime input result {} · source requirements {} · effective requirements {}",
        input.input_id, input.source_requirements_revision, input.effective_requirements_revision
    ));
    for resolution in &input.resolutions {
        lines.push(format!("Input {} disposition saved: {:?} · source requirements {} · execution evidence is separate",
            resolution.input_id, resolution.kind, resolution.source_requirements_revision));
    }
    if input.requirements_saved {
        lines.push(format!(
            "Requirements saved · affected tasks: {} · dispatch/adoption/verification are separate",
            input.affected_tasks.join(", ")
        ));
    }
    if let Some(control) = &input.control_saved {
        lines.push(format!(
            "Control intent saved: {} / {:?} · actual stop or execution facts are separate",
            control.target, control.intent
        ));
    }
    if input.control_superseded {
        lines.push(
            "Older input control was superseded; it did not overwrite the newer explicit control"
                .into(),
        );
    }
    if let Some(response) = &input.permission_response_saved {
        lines.push(format!(
            "Permission response saved: {} / {} · original input {} · transmission/tool effects are separate",
            response.decision_id, response.option_id, input.input_id
        ));
    }
    if let Some(answer) = &input.answer {
        lines.push(format!("BRO model answer: {answer}"));
    }
    if let Some(clarification) = &input.clarification {
        lines.push(format!(
            "Needs clarification / unavailable: {clarification}"
        ));
    }
    lines
}
