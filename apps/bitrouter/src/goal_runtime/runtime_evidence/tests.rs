use super::*;
use crate::supervisor::{SessionEvent, TurnState};

fn fixture(workspace: &Path) -> Result<(GoalSnapshot, Task, Execution, RunSnapshot, ReplayBatch)> {
    let mut goal: GoalSnapshot = serde_json::from_value(serde_json::json!({
        "id":"goal","owner":"local","title":"runtime evidence","workspace":workspace,
        "objective":"Deliver checked work","revision":7,"requirements_revision":2,
        "run_intent":"pause","created_at_ms":1,"updated_at_ms":2
    }))?;
    let execution: Execution = serde_json::from_value(serde_json::json!({
        "id":"execution","workspace":workspace,"requirements_revision":1,"task_revision":1,
        "observation":{"state":"running","run_id":"run","native_session_id":null}
    }))?;
    let mut task: Task = serde_json::from_value(serde_json::json!({
        "spec":{"id":"left","title":"left","instruction":"checked work","harness":"peer","dependencies":[],"checks":[]},
        "requirements_revision":1,"run_intent":"run","executions":[]
    }))?;
    task.executions.push(execution.clone());
    goal.workflow.tasks.push(task.clone());
    let snapshot: RunSnapshot = serde_json::from_value(serde_json::json!({
        "run_id":"run","label":"left","agent_id":"peer","cwd":workspace,"claim_key":workspace,
        "presentation":"background","process":"stopped","turn":"idle","attention":"none",
        "attachment":{"state":"detached"},"review":"unread","activity":"PRIVATE_ACTIVITY",
        "pending_permissions":[],"last_seq":4,
        "started_at":"2026-10-15T00:00:00Z","updated_at":"2026-10-15T00:00:04Z"
    }))?;
    let at = snapshot.started_at;
    let events = vec![
        SessionEvent {
            seq: 1,
            at,
            kind: SessionEventKind::UserPrompt {
                text: "PRIVATE_USER_INPUT".into(),
            },
        },
        SessionEvent {
            seq: 2,
            at: at + chrono::Duration::seconds(1),
            kind: SessionEventKind::Turn {
                turn: TurnState::Working,
            },
        },
        SessionEvent {
            seq: 3,
            at: at + chrono::Duration::seconds(2),
            kind: SessionEventKind::Activity {
                text: "PRIVATE_WORKER_OUTPUT".into(),
            },
        },
        SessionEvent {
            seq: 4,
            at: at + chrono::Duration::seconds(3),
            kind: SessionEventKind::TurnFailed {
                message: "PRIVATE_FAILURE_BODY".into(),
            },
        },
    ];
    let replay = ReplayBatch {
        first_retained_seq: 1,
        snapshot_seq: 4,
        history_complete: true,
        events,
    };
    Ok((goal, task, execution, snapshot, replay))
}

#[test]
fn runtime_metadata_binds_identity_and_never_copies_private_content() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let (goal, task, execution, snapshot, replay) = fixture(workspace.path())?;
    let value = project(&goal, &task, &execution, &snapshot, &replay)?;
    let encoded = serde_json::to_string(&value)?;
    assert!(!encoded.contains("PRIVATE_"));
    assert_eq!(value["requirements_revision"], 2);
    assert_eq!(value["execution_requirements_revision"], 1);
    assert_eq!(value["current_task_version"], true);
    assert_eq!(value["history_complete"], true);
    assert_eq!(value["worker_activity_markers"], 0);
    assert_eq!(value["admission_is_execution_evidence"], false);
    assert!(value["controller_confirmed_route"].is_null());
    for mutation in ["run", "harness", "workspace", "goal", "cutoff", "sequence"] {
        let mut changed_snapshot = snapshot.clone();
        let mut changed_goal = goal.clone();
        let mut changed_replay = replay.clone();
        let outside = tempfile::tempdir()?;
        match mutation {
            "run" => changed_snapshot.run_id = "other-run".into(),
            "harness" => changed_snapshot.agent_id = "other-harness".into(),
            "workspace" => changed_snapshot.cwd = outside.path().into(),
            "goal" => changed_goal.workflow.tasks.clear(),
            "cutoff" => changed_replay.snapshot_seq += 1,
            "sequence" => changed_replay.events[2].seq = 2,
            _ => anyhow::bail!("unknown fixture mutation"),
        }
        assert!(
            project(
                &changed_goal,
                &task,
                &execution,
                &changed_snapshot,
                &changed_replay
            )
            .is_err(),
            "accepted {mutation}"
        );
    }
    Ok(())
}

#[test]
fn actual_worker_updates_are_private_and_coalesced_only_within_their_turn() -> Result<()> {
    use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields};
    let workspace = tempfile::tempdir()?;
    let (goal, task, execution, mut snapshot, mut replay) = fixture(workspace.path())?;
    let mut fields = ToolCallUpdateFields::default();
    fields.raw_input = Some(serde_json::json!({"command":"PRIVATE_ARGUMENTS"}));
    let update = SessionEventKind::Update {
        update: SessionUpdate::ToolCallUpdate(ToolCallUpdate::new("PRIVATE_TOOL_ID", fields)),
    };
    replay.events.truncate(2);
    for seq in 3..=103 {
        replay.events.push(SessionEvent {
            seq,
            at: snapshot.started_at + chrono::Duration::seconds(seq as i64),
            kind: update.clone(),
        });
    }
    replay.events.push(SessionEvent {
        seq: 104,
        at: snapshot.started_at + chrono::Duration::seconds(104),
        kind: SessionEventKind::TurnFailed {
            message: "PRIVATE_FAILURE".into(),
        },
    });
    // A late update outside the pending turn supplies no new activity marker.
    replay.events.push(SessionEvent {
        seq: 105,
        at: snapshot.started_at + chrono::Duration::seconds(105),
        kind: update,
    });
    replay.snapshot_seq = 105;
    snapshot.last_seq = 105;
    let value = project(&goal, &task, &execution, &snapshot, &replay)?;
    assert_eq!(value["worker_activity_markers"], 1);
    assert_eq!(value["timeline_complete"], true);
    assert_eq!(value["omitted_timeline_events"], 0);
    assert_eq!(
        value["timeline"]
            .as_array()
            .context("timeline missing")?
            .len(),
        4
    );
    assert!(!serde_json::to_string(&value)?.contains("PRIVATE_"));
    Ok(())
}

#[test]
fn missing_omitted_unordered_or_bounded_history_is_never_complete_timing_proof() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let (goal, task, execution, mut snapshot, mut replay) = fixture(workspace.path())?;
    replay.history_complete = false;
    assert_eq!(
        project(&goal, &task, &execution, &snapshot, &replay)?["timeline_complete"],
        false
    );
    replay.history_complete = true;
    replay.events[2].kind = SessionEventKind::PayloadOmitted {
        description: "PRIVATE_OMITTED".into(),
    };
    assert_eq!(
        project(&goal, &task, &execution, &snapshot, &replay)?["history_complete"],
        false
    );
    replay.events[2].kind = SessionEventKind::Turn {
        turn: TurnState::Working,
    };
    replay.events[2].at = snapshot.started_at - chrono::Duration::seconds(1);
    assert_eq!(
        project(&goal, &task, &execution, &snapshot, &replay)?["time_ordered"],
        false
    );
    replay.events = (1..=128)
        .map(|seq| SessionEvent {
            seq,
            at: snapshot.started_at + chrono::Duration::seconds(seq as i64),
            kind: SessionEventKind::UserPrompt {
                text: "PRIVATE_INPUT".into(),
            },
        })
        .collect();
    replay.snapshot_seq = 128;
    snapshot.last_seq = 128;
    let value = project(&goal, &task, &execution, &snapshot, &replay)?;
    assert_eq!(
        value["timeline"]
            .as_array()
            .context("timeline missing")?
            .len(),
        64
    );
    assert_eq!(value["omitted_timeline_events"], 64);
    assert_eq!(value["timeline_complete"], false);
    assert!(!serde_json::to_string(&value)?.contains("PRIVATE_"));
    replay.events.clear();
    assert_eq!(
        project(&goal, &task, &execution, &snapshot, &replay)?["history_complete"],
        false
    );
    Ok(())
}
