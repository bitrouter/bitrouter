use super::*;
use crate::goal_store::DatabaseGoalStore;
use bitrouter_orchestrator::goal::GoalService;

fn pending(workspace: &Path, socket_id: &str) -> Pending {
    Pending {
        command: GoalCommand::Create {
            operation_id: socket_id.into(),
            title: "Long workflow".into(),
            workspace: workspace.into(),
            objective: "Produce a verified result".into(),
            budget_limits: Default::default(),
        },
        text: Some("Produce a verified result".into()),
        target: None,
    }
}

fn pending_input(goal_id: &str, operation_id: &str) -> Pending {
    Pending {
        command: GoalCommand::Input {
            operation_id: operation_id.into(),
            goal_id: goal_id.into(),
            expected_revision: 1,
            text: "change the requirement".into(),
            task_reference: None,
        },
        text: Some("change the requirement".into()),
        target: Some(goal_id.into()),
    }
}

#[test]
fn unresolved_input_blocks_only_its_lane_and_never_blocks_explicit_stop() -> Result<()> {
    let mut admissions = submissions::Submissions::default();
    admissions.retain(pending_input("a", "original-a"))?;
    let mut state = State::new("/project".into());
    state.observe("a".into());
    admissions.sync(&mut state);
    assert!(state.notebook().is_some_and(|book| book.pending));
    assert!(!admissions.begin(&Effect::Submit {
        goal_id: "a".into(),
        text: "duplicate".into()
    }));
    for index in 0..8 {
        assert!(admissions.begin(&Effect::Submit {
            goal_id: format!("other-{index}"),
            text: "independent".into()
        }));
    }
    assert!(!admissions.begin(&Effect::Submit {
        goal_id: "ninth".into(),
        text: "bounded".into()
    }));
    assert!(admissions.begin(&Effect::Control {
        goal_id: "a".into(),
        intent: Control::Stop
    }));
    assert!(!admissions.begin(&Effect::Control {
        goal_id: "a".into(),
        intent: Control::Run
    }));
    let control = Effect::Control {
        goal_id: "b".into(),
        intent: Control::Pause,
    };
    assert!(admissions.begin(&control));
    state.observe("b".into());
    admissions.sync(&mut state);
    assert!(state.notebook().is_some_and(|book| !book.pending));
    admissions.finish_effect(&control);
    Ok(())
}

#[test]
fn receipt_recovery_rotates_lanes_without_replaying_or_replacing_identity() -> Result<()> {
    let mut admissions = submissions::Submissions::default();
    admissions.retain(pending_input("a", "first-a"))?;
    admissions.retain(pending_input("a", "second-a"))?;
    admissions.retain(pending_input("b", "first-b"))?;
    let a = admissions.next()?.context("missing a lookup")?;
    let b = admissions.next()?.context("missing b lookup")?;
    assert_eq!(a.id()?, "first-a");
    assert_eq!(b.id()?, "first-b");
    assert!(admissions.next()?.is_none());
    // Unknown acceptance goes to the back; it never acquires a new operation ID.
    admissions.retain(a)?;
    admissions.finish(&b)?;
    let next = admissions.next()?.context("missing second a lookup")?;
    assert_eq!(next.id()?, "second-a");
    admissions.finish(&next)?;
    let original = admissions.next()?.context("original identity lost")?;
    assert_eq!(original.id()?, "first-a");
    Ok(())
}

#[test]
fn old_receipt_with_identical_text_cannot_clear_a_different_pending_draft() -> Result<()> {
    let mut state = State::new("/project".into());
    state.observe("a".into());
    let original = pending_input("a", "old-input");
    let current = pending_input("a", "current-input");
    if let Some(book) = state.notebooks.get_mut("a") {
        book.editor.set_text("change the requirement");
    }
    let mut drafts = BTreeMap::from([(Some("a".into()), "current-input".into())]);
    assert!(!accept_draft(&mut state, &mut drafts, &original)?);
    assert_eq!(
        state.notebook().map(|book| book.editor.text()),
        Some("change the requirement")
    );
    assert!(accept_draft(&mut state, &mut drafts, &current)?);
    assert!(state.notebook().is_some_and(|book| book.editor.is_empty()));
    Ok(())
}

#[tokio::test]
async fn permission_preview_uses_actual_harness_request_bytes_and_never_reports_execution()
-> Result<()> {
    use agent_client_protocol::schema::v1::{
        PermissionOption, PermissionOptionKind, ToolCallUpdate, ToolCallUpdateFields,
    };
    let temporary = tempfile::tempdir()?;
    let mut fields = ToolCallUpdateFields::default();
    fields.title = Some("Run the requested check".into());
    fields.raw_input = Some(serde_json::json!({"command":"cargo fmt --check"}));
    let request = crate::supervisor::PendingPermissionSnapshot {
        permission_id: "actual-permission".into(),
        tool_call: ToolCallUpdate::new("actual-tool", fields),
        options: vec![PermissionOption::new(
            "once",
            "Allow once",
            PermissionOptionKind::AllowOnce,
        )],
    };
    let reference =
        crate::goal_artifacts::put(temporary.path(), &serde_json::to_vec(&request)?).await?;
    let bytes = crate::goal_artifacts::read(temporary.path(), &reference).await?;
    let text = projection::permission_text(&bytes)?;
    assert!(text.contains("Requested operation (harness reported): Run the requested check"));
    assert!(text.contains("Kind (harness reported): unreported"));
    assert!(text.contains("cargo fmt --check"));
    assert!(text.contains("actual-permission"));
    assert!(projection::permission_text(b"not a supported request").is_err());
    tokio::fs::write(&reference.path, b"changed request").await?;
    assert!(
        crate::goal_artifacts::read(temporary.path(), &reference)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn outbox_retains_exact_identity_scope_and_text_without_replaying() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let socket = temporary.path().join("control.sock");
    let outbox = outbox::Outbox::new(temporary.path().join("outbox"), socket.clone());
    let original = pending(temporary.path(), &uuid::Uuid::new_v4().to_string());
    outbox.save(&original).await?;
    outbox.save(&original).await?;
    let restored = outbox.recover().await?;
    assert_eq!(restored.len(), 1);
    let saved = restored.first().context("missing pending operation")?;
    assert_eq!(
        serde_json::to_value(&saved.command)?,
        serde_json::to_value(&original.command)?
    );
    assert_eq!(saved.text, original.text);
    assert_eq!(saved.target, original.target);
    let other = outbox::Outbox::new(
        temporary.path().join("outbox"),
        temporary.path().join("different.sock"),
    );
    assert!(other.recover().await?.is_empty());
    // No daemon or worker is involved: reading an outbox never sends its data.
    outbox.remove(&original).await?;
    assert!(outbox.recover().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn changed_protocol_outbox_is_preserved_without_reinterpreting_or_replaying() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let outbox = outbox::Outbox::new(
        temporary.path().join("outbox"),
        temporary.path().join("local.sock"),
    );
    let original = pending(temporary.path(), &uuid::Uuid::new_v4().to_string());
    outbox.save(&original).await?;
    let path = temporary
        .path()
        .join("outbox")
        .join(format!("{}.json", original.id()?));
    let mut old: serde_json::Value = serde_json::from_slice(&tokio::fs::read(&path).await?)?;
    old["protocol"] = serde_json::json!(1);
    if let Some(command) = old["pending"]["command"].as_object_mut() {
        command.remove("budget_limits");
    }
    let bytes = serde_json::to_vec(&old)?;
    tokio::fs::write(&path, &bytes).await?;
    assert!(outbox.recover().await.is_err());
    assert_eq!(tokio::fs::read(path).await?, bytes);
    Ok(())
}

#[tokio::test]
async fn actual_database_receipt_must_match_the_original_client_payload() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let service = GoalService::new(std::sync::Arc::new(DatabaseGoalStore::new(db)));
    let temporary = tempfile::tempdir()?;
    let original = pending(temporary.path(), &uuid::Uuid::new_v4().to_string());
    let GoalReply::Accepted { operation } = service
        .execute("local", original.command.clone(), 100)
        .await?
    else {
        anyhow::bail!("missing actual receipt");
    };
    original.verifies(&operation)?;
    let mut other = operation.clone();
    other.fingerprint = "wrong-source".into();
    assert!(original.verifies(&other).is_err());
    let mut other = operation;
    other.id = uuid::Uuid::new_v4().to_string();
    assert!(original.verifies(&other).is_err());
    Ok(())
}

#[tokio::test]
async fn real_goal_client_refuses_stale_budget_forms_and_runtime_refuses_racing_updates()
-> Result<()> {
    use crate::daemon::{
        AcpControlPlane, DaemonCommand, DaemonResponse, LocalControlOptions, NoopObserveStatus,
        NoopReloader,
    };
    use std::sync::Arc;
    let temporary = tempfile::tempdir()?;
    let socket = temporary.path().join("goal-client.sock");
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let service = Arc::new(GoalService::new(Arc::new(DatabaseGoalStore::new(
        db.clone(),
    ))));
    let bound = crate::daemon::bind_control_socket(&socket).await?;
    let running = tokio::spawn(crate::daemon::run_bound_control_socket(
        bound,
        Arc::new(bitrouter_sdk::App::builder().build()?),
        "127.0.0.1:0".into(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
        AcpControlPlane {
            runtime: Arc::new(crate::acp_runtime::AcpRuntime::new()),
            metering: crate::metering::MeteringStore::new(db),
            inventory: None,
            evolution: None,
        },
        LocalControlOptions {
            administration: None,
            goals: Some(service),
            gate: crate::daemon_handoff::HandoffGate::default(),
            cli_owned: false,
        },
    ));
    let client = Client::new(socket.clone());
    let original = pending(temporary.path(), &uuid::Uuid::new_v4().to_string());
    let GoalReply::Accepted { operation } = client.request(original.command).await? else {
        anyhow::bail!("missing creation receipt");
    };
    let id = operation.goal_id;
    let form = Effect::SetBudget {
        goal_id: id.clone(),
        original: Default::default(),
        limits: bitrouter_tui::goals::budget::Limits {
            elapsed_ms: Some(60_000),
            managed_token_estimates_micro_usd: Some(1_000_000),
        },
    };
    let prepared = client::prepare(&client, temporary.path(), &form).await?;
    let changed = bitrouter_orchestrator::goal::budget::Limits {
        elapsed_ms: None,
        managed_token_estimates_micro_usd: Some(2_000_000),
    };
    client
        .request(GoalCommand::SetBudget {
            operation_id: uuid::Uuid::new_v4().to_string(),
            goal_id: id.clone(),
            expected_revision: operation.revision,
            limits: changed.clone(),
        })
        .await?;
    let race = client.request(prepared.command).await;
    assert!(race.as_ref().is_err_and(|error| {
        error.downcast_ref::<bitrouter_orchestrator::goal::GoalError>()
            == Some(&bitrouter_orchestrator::goal::GoalError::Conflict)
    }));
    assert!(
        client::prepare(&client, temporary.path(), &form)
            .await
            .is_err()
    );
    let GoalReply::Snapshot { goal } = client
        .request(GoalCommand::Get {
            goal_id: id.clone(),
        })
        .await?
    else {
        anyhow::bail!("missing snapshot");
    };
    assert_eq!(goal.budget.limits, changed);
    assert_eq!(goal.revision, 2);
    let GoalReply::History { page } = client
        .request(GoalCommand::History {
            goal_id: id,
            after: 0,
            cutoff: None,
            limit: 16,
        })
        .await?
    else {
        anyhow::bail!("missing history");
    };
    assert_eq!(page.operations.len(), 2);
    assert!(matches!(
        crate::daemon::send_command(&socket, &DaemonCommand::Stop).await?,
        DaemonResponse::Ok
    ));
    tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .context("Goal client service did not stop")???;
    Ok(())
}

#[tokio::test]
async fn exact_withdrawal_uses_real_client_outbox_and_no_model_under_budget_halt() -> Result<()> {
    use crate::daemon::{
        AcpControlPlane, DaemonCommand, DaemonResponse, LocalControlOptions, NoopObserveStatus,
        NoopReloader,
    };
    use bitrouter_orchestrator::goal::input::{Event as InputEvent, Interpretation, Stage};
    use bitrouter_orchestrator::goal::workflow::{Change, OWNER_LEASE_MS, WorkflowActor};
    use std::sync::Arc;
    let temporary = tempfile::tempdir()?;
    let socket = temporary.path().join("withdraw-client.sock");
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let service = Arc::new(GoalService::new(Arc::new(DatabaseGoalStore::new(
        db.clone(),
    ))));
    let bound = crate::daemon::bind_control_socket(&socket).await?;
    let running = tokio::spawn(crate::daemon::run_bound_control_socket(
        bound,
        Arc::new(bitrouter_sdk::App::builder().build()?),
        "127.0.0.1:0".into(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
        AcpControlPlane {
            runtime: Arc::new(crate::acp_runtime::AcpRuntime::new()),
            metering: crate::metering::MeteringStore::new(db),
            inventory: None,
            evolution: None,
        },
        LocalControlOptions {
            administration: None,
            goals: Some(service.clone()),
            gate: crate::daemon_handoff::HandoffGate::default(),
            cli_owned: false,
        },
    ));
    let client = Client::new(socket.clone());
    let GoalReply::Accepted { operation } = client
        .request(pending(temporary.path(), "withdraw-goal").command)
        .await?
    else {
        anyhow::bail!("missing creation");
    };
    let id = operation.goal_id;
    let time = bitrouter_orchestrator::store::owner_time_ms().map_err(anyhow::Error::msg)?;
    service
        .workflow_change(
            WorkflowActor {
                owner: "local",
                coordinator_id: "withdraw-test",
            },
            &id,
            "withdraw-owner",
            1,
            Change::ClaimCoordinator {
                id: "withdraw-test".into(),
                expires_at_ms: time + OWNER_LEASE_MS,
            },
            time,
        )
        .await?;
    let GoalReply::Snapshot { goal } = client
        .request(GoalCommand::Get {
            goal_id: id.clone(),
        })
        .await?
    else {
        anyhow::bail!("missing snapshot");
    };
    let GoalReply::Accepted { operation } = client
        .request(GoalCommand::Input {
            operation_id: "withdraw-question".into(),
            goal_id: id.clone(),
            expected_revision: goal.revision,
            text: "Add an ambiguous requirement".into(),
            task_reference: None,
        })
        .await?
    else {
        anyhow::bail!("missing input");
    };
    for (key, event) in [
        (
            "question-begin",
            InputEvent::Begin {
                operation: Box::new(operation),
                attempt_id: "question-understanding".into(),
                stage: Stage::Reasoning,
            },
        ),
        (
            "question-finish",
            InputEvent::Finish {
                attempt_id: "question-understanding".into(),
                interpretation: Interpretation {
                    clarification: Some("Which requirement?".into()),
                    ..Default::default()
                },
                reasoning_request_id: Some("fixture-reasoning".into()),
            },
        ),
    ] {
        service
            .workflow_change(
                WorkflowActor {
                    owner: "local",
                    coordinator_id: "withdraw-test",
                },
                &id,
                key,
                1,
                Change::Input { event },
                time,
            )
            .await?;
    }
    let GoalReply::Snapshot { goal } = client
        .request(GoalCommand::Get {
            goal_id: id.clone(),
        })
        .await?
    else {
        anyhow::bail!("missing snapshot");
    };
    client
        .request(GoalCommand::SetBudget {
            operation_id: "halt-for-withdrawal".into(),
            goal_id: id.clone(),
            expected_revision: goal.revision,
            limits: bitrouter_orchestrator::goal::budget::Limits {
                elapsed_ms: Some(1),
                managed_token_estimates_micro_usd: None,
            },
        })
        .await?;
    let effect = Effect::Submit {
        goal_id: id.clone(),
        text: "/input withdraw-question /withdraw".into(),
    };
    let prepared = client::prepare(&client, temporary.path(), &effect).await?;
    assert!(
        matches!(&prepared.command, GoalCommand::WithdrawInput { input_id, source_requirements_revision: 1, .. } if input_id == "withdraw-question")
    );
    let mut admissions = submissions::Submissions::default();
    assert!(admissions.begin(&effect));
    let outbox = outbox::Outbox::new(temporary.path().join("outbox"), socket.clone());
    outbox.save(&prepared).await?;
    let mut recovered = outbox.recover().await?;
    assert_eq!(recovered.len(), 1);
    let saved = recovered.remove(0);
    assert_eq!(saved.text, prepared.text);
    let GoalReply::Accepted { operation } = client.request(saved.command.clone()).await? else {
        anyhow::bail!("missing withdrawal");
    };
    saved.verifies(&operation)?;
    admissions.finish(&saved)?;
    assert!(
        admissions.begin(&effect),
        "withdrawal lane remained stuck after receipt"
    );
    outbox.remove(&saved).await?;
    assert!(outbox.recover().await?.is_empty());
    let GoalReply::Snapshot { goal } = client
        .request(GoalCommand::Get {
            goal_id: id.clone(),
        })
        .await?
    else {
        anyhow::bail!("missing snapshot");
    };
    assert!(goal.inputs.unresolved_inputs().is_empty());
    assert_eq!(goal.requirements_revision, 1);
    assert_eq!(goal.accounting.provider_attempts, 0);
    assert!(
        goal.inputs
            .latest
            .as_ref()
            .is_some_and(|result| result.reasoning_request_id.is_none())
    );
    let natural = client::prepare(
        &client,
        temporary.path(),
        &Effect::Submit {
            goal_id: id,
            text: "If I withdraw it, what happens?".into(),
        },
    )
    .await?;
    assert!(matches!(natural.command, GoalCommand::Input { .. }));
    assert!(matches!(
        crate::daemon::send_command(&socket, &DaemonCommand::Stop).await?,
        DaemonResponse::Ok
    ));
    tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .context("withdrawal client service did not stop")???;
    Ok(())
}

#[tokio::test]
async fn projection_keeps_an_unresolved_question_visible_after_an_unrelated_answer() -> Result<()> {
    use bitrouter_orchestrator::goal::input::{ClarificationKind, UnresolvedInput};
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let service = GoalService::new(std::sync::Arc::new(DatabaseGoalStore::new(db)));
    let workspace = tempfile::tempdir()?;
    let GoalReply::Accepted { operation } = service
        .execute(
            "local",
            pending(workspace.path(), "projection-clarification").command,
            100,
        )
        .await?
    else {
        anyhow::bail!("missing creation");
    };
    let GoalReply::Snapshot { mut goal } = service
        .execute(
            "local",
            GoalCommand::Get {
                goal_id: operation.goal_id,
            },
            110,
        )
        .await?
    else {
        anyhow::bail!("missing snapshot");
    };
    goal.inputs.unresolved.push(UnresolvedInput {
        input_id: "original-change".into(),
        source_requirements_revision: 1,
        task_reference: None,
        permission_decision: None,
        question: "Which additional result do you require?".into(),
        kind: ClarificationKind::Change,
    });
    goal.inputs.latest = Some(serde_json::from_value(serde_json::json!({
        "input_id":"unrelated-query","source_requirements_revision":1,"effective_requirements_revision":1,
        "answer":"Original work was checked","clarification":null,"requirements_saved":false,"affected_tasks":[],
        "control_saved":null,"controlled_tasks":[],"control_superseded":false,"classification":null,"reasoning_request_id":"query-model-request"
    }))?);
    let projected = projection::goal(&goal, &mut projection::Artifacts::new());
    assert!(projected.needs_you);
    assert!(projected.reason.contains("Completion waits"));
    let lines = projection::input_lines(&goal);
    assert!(
        lines.iter().any(
            |line| line.contains("original-change") && line.contains("Which additional result")
        )
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Original work was checked"))
    );
    goal.inputs.unresolved[0].kind = ClarificationKind::Query;
    assert!(!goal.inputs.completion_waits_for_input());
    assert!(
        projection::input_lines(&goal)
            .iter()
            .any(|line| line.contains("does not block completion"))
    );
    Ok(())
}

#[tokio::test]
async fn projection_preserves_incomplete_budget_and_unknown_effects_as_separate_dimensions()
-> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let service = GoalService::new(std::sync::Arc::new(DatabaseGoalStore::new(db)));
    let temporary = tempfile::tempdir()?;
    let original = pending(temporary.path(), &uuid::Uuid::new_v4().to_string());
    let GoalReply::Accepted { operation } = service.execute("local", original.command, 100).await?
    else {
        anyhow::bail!("missing creation");
    };
    let GoalReply::Snapshot { mut goal } = service
        .execute(
            "local",
            GoalCommand::Get {
                goal_id: operation.goal_id,
            },
            110,
        )
        .await?
    else {
        anyhow::bail!("missing snapshot");
    };
    goal.budget.resume_required = true;
    goal.budget.halt = Some("Budget reached; execution confirmation is separate".into());
    let mut artifacts = projection::Artifacts::new();
    let data = projection::goal(&goal, &mut artifacts);
    assert_eq!(data.satisfaction, "incomplete");
    assert!(data.reason.contains("Budget reached"));
    assert!(data.execution.contains("No worker execution"));
    assert!(
        data.budget
            .iter()
            .any(|line| line.contains("may be unknown"))
    );
    assert_eq!(data.delivery, "None");
    assert!(data.acceptance.is_none());
    goal.budget.resume_required = false;
    goal.budget.halt = None;
    goal.workflow.planning = bitrouter_orchestrator::goal::workflow::Planning::Planned {
        attempt_id: "projection-fixture".into(),
    };
    goal.workflow.tasks.push(serde_json::from_value(serde_json::json!({
        "spec":{"id":"left","title":"Left","instruction":"Retained task","harness":"configured",
            "dependencies":[],"checks":[]},"requirements_revision":1,"executions":[{
                "id":"prior","workspace":temporary.path(),"observation":{"state":"unknown","run_id":"actual-run","reason":"Old tool outcome unknown"},
                "requirements_revision":1,"task_revision":1,"dispatch_requested":true,
                "stop":{"state":"confirmed","operation_id":"stop","detail":"Owned process stopped"},
                "reconciliation":{"state":"blocked","task_revision":1,"control_source_revision":1,"reason":"Tool status still unresolved; no replacement"}
            }]
    }))?);
    let blocked = projection::goal(&goal, &mut artifacts);
    assert!(blocked.needs_you);
    assert_ne!(blocked.satisfaction, "complete");
    assert_eq!(blocked.delivery, "None");
    assert!(
        blocked.tasks[0]
            .details
            .iter()
            .any(|line| line.contains("no replacement"))
    );
    Ok(())
}
