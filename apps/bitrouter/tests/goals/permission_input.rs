//! Runtime authority and transaction tests; model proposals are fixture data.
use super::decision_tests::pending;
use super::*;
use bitrouter_orchestrator::goal::decision::PermissionState;
use bitrouter_orchestrator::goal::input::{
    Event, Interpretation, PermissionResponse, Stage, permission_reference,
};

async fn begin_reply(
    goals: &GoalService,
    goal_id: &str,
    text: &str,
    stage: Stage,
) -> Result<GoalOperation> {
    let head = snapshot(goals, goal_id).await?;
    let operation = accepted(
        goals
            .execute(
                "local",
                GoalCommand::Input {
                    operation_id: "original-reply".into(),
                    goal_id: goal_id.into(),
                    expected_revision: head.revision,
                    text: text.into(),
                    task_reference: None,
                },
                100,
            )
            .await?,
    )?;
    goals
        .workflow_change(
            actor(),
            goal_id,
            "reply-begin",
            1,
            Change::Input {
                event: Event::Begin {
                    operation: Box::new(operation.clone()),
                    attempt_id: "reply-attempt".into(),
                    stage,
                },
            },
            110,
        )
        .await?;
    Ok(operation)
}

fn proposal(decision_id: &str, option_id: &str) -> Interpretation {
    Interpretation {
        answer: None,
        objective: None,
        task_updates: Vec::new(),
        control: None,
        permission_response: Some(PermissionResponse {
            decision_id: decision_id.into(),
            option_id: option_id.into(),
        }),
        clarification_kind: None,
        resolutions: Vec::new(),
        modification_scope: None,
        clarification: None,
    }
}

async fn finish_reply(
    goals: &GoalService,
    goal_id: &str,
    interpretation: Interpretation,
) -> std::result::Result<GoalOperation, GoalError> {
    goals
        .workflow_change(
            actor(),
            goal_id,
            "reply-finish",
            1,
            Change::Input {
                event: Event::Finish {
                    attempt_id: "reply-attempt".into(),
                    interpretation,
                    reasoning_request_id: Some("fixture-understanding-request".into()),
                },
            },
            130,
        )
        .await
}

#[test]
fn permission_scope_comes_from_the_original_explicit_reference() -> Result<()> {
    assert!(permission_reference("可以")?.is_none());
    assert!(permission_reference("> /decision another-request 允许")?.is_none());
    assert!(permission_reference("Explain /decision another-request")?.is_none());
    let reference =
        permission_reference("/decision actual-request /option actual-option 允许这个选项")?
            .context("explicit reference")?;
    assert_eq!(reference.decision_id, "actual-request");
    assert_eq!(reference.option_id, Some("actual-option"));
    assert!(permission_reference("/decision").is_err());
    assert!(permission_reference("/decision actual-request /option").is_err());
    Ok(())
}

#[tokio::test]
async fn reply_is_saved_with_original_input_identity_before_permission_delivery() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let (id, decision) = pending(&goals, workspace.path()).await?;
    let original = begin_reply(
        &goals,
        &id,
        "/decision permission-decision 允许这一次",
        Stage::Reasoning,
    )
    .await?;
    let response = proposal(&decision.id, "offered-once");
    let first = finish_reply(&goals, &id, response.clone()).await?;
    let second = finish_reply(&service(db.clone()), &id, response).await?;
    assert_eq!(first, second);
    let restored = snapshot(&service(db), &id).await?;
    assert_eq!(restored.requirements_revision, 1);
    assert_eq!(restored.run_intent, RunIntent::Run);
    assert!(matches!(&restored.workflow.decisions[0].state,
        PermissionState::Requested { operation_id, option_id, automatic: false, dispatch_requested: false }
        if operation_id == &original.id && option_id == "offered-once"));
    let result = restored.inputs.latest.context("saved input result")?;
    assert_eq!(result.input_id, original.id);
    assert!(!result.requirements_saved);
    assert!(result.control_saved.is_none());
    assert_eq!(
        result.permission_response_saved,
        Some(PermissionResponse {
            decision_id: decision.id,
            option_id: "offered-once".into(),
        })
    );
    Ok(())
}

#[tokio::test]
async fn asking_about_a_permission_does_not_choose_an_option() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let (id, _) = pending(&goals, workspace.path()).await?;
    begin_reply(
        &goals,
        &id,
        "/decision permission-decision 为什么需要这个权限？",
        Stage::Reasoning,
    )
    .await?;
    let mut answer = proposal("permission-decision", "offered-once");
    answer.permission_response = None;
    answer.answer = Some("This is a fixture explanation, not consent".into());
    finish_reply(&goals, &id, answer).await?;
    let head = snapshot(&goals, &id).await?;
    assert_eq!(head.workflow.decisions[0].state, PermissionState::Pending);
    assert!(
        head.inputs
            .latest
            .context("query result")?
            .permission_response_saved
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn unscoped_foreign_unoffered_and_cross_scope_proposals_cannot_grant_permission() -> Result<()>
{
    for (text, decision_id, option_id, changes_goal) in [
        ("可以", "permission-decision", "offered-once", false),
        (
            "/decision permission-decision 允许",
            "foreign-decision",
            "offered-once",
            false,
        ),
        (
            "/decision permission-decision 允许",
            "permission-decision",
            "invented-option",
            false,
        ),
        (
            "/decision permission-decision 允许",
            "permission-decision",
            "offered-once",
            true,
        ),
    ] {
        let goals = service(database().await?);
        let workspace = tempfile::tempdir()?;
        let (id, _) = pending(&goals, workspace.path()).await?;
        begin_reply(&goals, &id, text, Stage::Reasoning).await?;
        let original = snapshot(&goals, &id).await?;
        let mut response = proposal(decision_id, option_id);
        if changes_goal {
            response.objective = Some("Replace unrelated Goal requirements".into());
        }
        assert!(matches!(
            finish_reply(&goals, &id, response).await,
            Err(GoalError::Invalid { .. })
        ));
        assert_eq!(snapshot(&goals, &id).await?, original);
    }
    Ok(())
}

#[tokio::test]
async fn persistent_allow_requires_the_explicit_offered_option_identity() -> Result<()> {
    for (text, succeeds) in [
        ("/decision permission-decision 可以", false),
        (
            "/decision permission-decision /option offered-once 允许",
            false,
        ),
        (
            "/decision permission-decision /option offered-always 允许这个持久选项",
            true,
        ),
    ] {
        let goals = service(database().await?);
        let workspace = tempfile::tempdir()?;
        let (id, _) = pending(&goals, workspace.path()).await?;
        begin_reply(&goals, &id, text, Stage::Reasoning).await?;
        let result = finish_reply(
            &goals,
            &id,
            proposal("permission-decision", "offered-always"),
        )
        .await;
        assert_eq!(result.is_ok(), succeeds);
        assert!(
            matches!(snapshot(&goals, &id).await?.workflow.decisions[0].state,
            PermissionState::Requested { .. } if succeeds)
                || !succeeds
                    && snapshot(&goals, &id).await?.workflow.decisions[0].state
                        == PermissionState::Pending
        );
    }
    Ok(())
}

#[tokio::test]
async fn typed_classification_cannot_grant_permission_even_with_a_valid_reference() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let (id, _) = pending(&goals, workspace.path()).await?;
    begin_reply(
        &goals,
        &id,
        "/decision permission-decision 允许这一次",
        Stage::Typed,
    )
    .await?;
    assert!(matches!(
        finish_reply(&goals, &id, proposal("permission-decision", "offered-once")).await,
        Err(GoalError::Invalid { .. })
    ));
    assert_eq!(
        snapshot(&goals, &id).await?.workflow.decisions[0].state,
        PermissionState::Pending
    );
    Ok(())
}

#[tokio::test]
async fn paused_goal_rejects_late_allow_but_can_save_an_explicit_denial() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let (id, _) = pending(&goals, workspace.path()).await?;
    begin_reply(
        &goals,
        &id,
        "/decision permission-decision 拒绝这个请求",
        Stage::Reasoning,
    )
    .await?;
    let head = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "pause-during-understanding".into(),
                goal_id: id.clone(),
                expected_revision: head.revision,
                intent: RunIntent::Pause,
            },
            120,
        )
        .await?;
    assert!(matches!(
        finish_reply(&goals, &id, proposal("permission-decision", "offered-once")).await,
        Err(GoalError::Conflict)
    ));
    finish_reply(
        &goals,
        &id,
        proposal("permission-decision", "offered-reject"),
    )
    .await?;
    assert_eq!(snapshot(&goals, &id).await?.run_intent, RunIntent::Pause);
    Ok(())
}

#[tokio::test]
async fn permission_reply_persistence_failure_rolls_back_the_choice_and_input_result() -> Result<()>
{
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let (id, _) = pending(&goals, workspace.path()).await?;
    begin_reply(
        &goals,
        &id,
        "/decision permission-decision 允许这一次",
        Stage::Reasoning,
    )
    .await?;
    let original = snapshot(&goals, &id).await?;
    db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "CREATE TRIGGER reject_permission_reply BEFORE INSERT ON bro_goal_operations WHEN NEW.id = 'reply-finish' BEGIN SELECT RAISE(ABORT, 'fixture permission reply persistence failure'); END".to_owned())).await?;
    assert!(matches!(
        finish_reply(&goals, &id, proposal("permission-decision", "offered-once")).await,
        Err(GoalError::Storage { .. })
    ));
    assert_eq!(snapshot(&service(db), &id).await?, original);
    Ok(())
}

#[tokio::test]
async fn changed_requirements_finish_the_reply_without_granting_the_old_request() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let (id, _) = pending(&goals, workspace.path()).await?;
    begin_reply(
        &goals,
        &id,
        "/decision permission-decision 允许这一次",
        Stage::Reasoning,
    )
    .await?;
    let mut head = snapshot(&goals, &id).await?;
    let expected = head.revision;
    head.revision += 1;
    head.requirements_revision = 2;
    let operation = GoalOperation {
        id: "fixture-new-requirements".into(),
        fingerprint: "fixture-version-change".into(),
        goal_id: id.clone(),
        revision: head.revision,
        requirements_revision: 2,
        accepted_at_ms: 120,
        kind: OperationKind::Input {
            text: "Fixture: requirements changed during understanding".into(),
            task_reference: None,
        },
    };
    DatabaseGoalStore::new(db)
        .commit(Some(expected), head, operation)
        .await?;
    finish_reply(&goals, &id, proposal("permission-decision", "offered-once")).await?;
    let head = snapshot(&goals, &id).await?;
    assert_eq!(head.workflow.decisions[0].state, PermissionState::Pending);
    assert_eq!(head.requirements_revision, 2);
    let result = head.inputs.latest.context("stale reply disposition")?;
    assert!(result.permission_response_saved.is_none());
    assert!(result.clarification.is_some());
    assert!(head.inputs.pending.is_none());
    Ok(())
}

#[tokio::test]
async fn exhausted_budget_rejects_late_allow_without_blocking_a_recorded_denial() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let (id, _) = pending(&goals, workspace.path()).await?;
    begin_reply(
        &goals,
        &id,
        "/decision permission-decision 拒绝这个请求",
        Stage::Reasoning,
    )
    .await?;
    let head = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::SetBudget {
                operation_id: "budget-during-understanding".into(),
                goal_id: id.clone(),
                expected_revision: head.revision,
                limits: bitrouter_orchestrator::goal::budget::Limits {
                    elapsed_ms: Some(1),
                    managed_token_estimates_micro_usd: None,
                },
            },
            120,
        )
        .await?;
    assert!(matches!(
        finish_reply(&goals, &id, proposal("permission-decision", "offered-once")).await,
        Err(GoalError::BudgetExceeded { .. })
    ));
    finish_reply(
        &goals,
        &id,
        proposal("permission-decision", "offered-reject"),
    )
    .await?;
    assert!(snapshot(&goals, &id).await?.budget.resume_required);
    Ok(())
}
