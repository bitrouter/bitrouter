use super::*;
use crate::goal::completion::{Attempt, State};
use crate::goal::decision::{Event, PermissionOption, apply};
use crate::goal::verification::CheckRecipe;
use crate::goal::workflow::{CheckRun, CoordinatorLease};

fn pending() -> (GoalSnapshot, PermissionDecision) {
    let workspace = std::env::temp_dir().join("command-permission-fixture");
    let artifact = ArtifactRef {
        path: workspace.join("artifact"),
        sha256: "a".repeat(64),
        bytes: 1,
    };
    let spec = CheckSpec {
        id: "cargo".into(),
        description: "Run Cargo tests".into(),
        recipe: CheckRecipe::CargoTest,
        timeout_seconds: 30,
    };
    let mut goal = GoalSnapshot {
        id: "goal".into(),
        owner: "local".into(),
        title: "Command permission".into(),
        workspace: workspace.clone(),
        objective: "Verify delivery".into(),
        revision: 1,
        requirements_revision: 1,
        run_intent: RunIntent::Run,
        created_at_ms: 1,
        updated_at_ms: 1,
        workflow: Default::default(),
        inputs: Default::default(),
        control_source_revision: 1,
        accounting: Default::default(),
        budget: Default::default(),
    };
    goal.workflow.coordinator = Some(CoordinatorLease {
        id: "owner".into(),
        expires_at_ms: 100,
    });
    goal.workflow.completion.attempt = Some(Attempt {
        id: "delivery".into(),
        requirements_revision: 1,
        sources: vec![],
        workspace: workspace.clone(),
        base_commit: Some("base".into()),
        delivery: Some(artifact.clone()),
        checks: vec![CheckRun {
            id: "check-run".into(),
            spec: spec.clone(),
            requirements_revision: 1,
            task_revision: 1,
            workspace: artifact.clone(),
            state: CheckState::Requested,
        }],
        assessment: None,
        accepted: false,
        state: State::Checking,
    });
    let decision = PermissionDecision {
        id: "decision".into(),
        execution_id: "delivery".into(),
        run_id: None,
        check: Some(Box::new(CheckPermission {
            specification: spec,
            workspace,
            artifact: artifact.clone(),
            coordinator_id: "owner".into(),
            control_source_revision: 1,
        })),
        permission_id: "check-run".into(),
        requirements_revision: 1,
        task_revision: 0,
        request: artifact,
        options: vec![
            PermissionOption {
                id: "allow-once".into(),
                label: "Allow once".into(),
                allows: true,
                persistent: false,
            },
            PermissionOption {
                id: "deny-once".into(),
                label: "Deny".into(),
                allows: false,
                persistent: false,
            },
        ],
        state: PermissionState::Pending,
    };
    (goal, decision)
}

#[test]
fn explicit_check_consent_is_bound_and_consumed() -> Result<(), GoalError> {
    let (mut goal, decision) = pending();
    apply(
        &mut goal,
        &Event::Observe {
            decision: Box::new(decision),
        },
    )?;
    assert!(authorize(&goal, "decision", 2).is_err());
    assert!(
        apply(
            &mut goal,
            &Event::Respond {
                decision_id: "decision".into(),
                operation_id: "auto".into(),
                option_id: "allow-once".into(),
                automatic: true
            }
        )
        .is_err()
    );
    apply(
        &mut goal,
        &Event::Respond {
            decision_id: "decision".into(),
            operation_id: "explicit".into(),
            option_id: "allow-once".into(),
            automatic: false,
        },
    )?;
    assert!(authorize(&goal, "decision", 2).is_err());
    apply(
        &mut goal,
        &Event::RequestDelivery {
            decision_id: "decision".into(),
            operation_id: "explicit".into(),
        },
    )?;
    assert!(authorize(&goal, "decision", 2).is_ok());
    assert!(authorize(&goal, "decision", 100).is_err());
    for mutation in [
        "requirements",
        "control",
        "pause",
        "owner",
        "budget",
        "recipe",
        "artifact",
        "workspace",
    ] {
        let mut changed = goal.clone();
        match mutation {
            "requirements" => changed.requirements_revision += 1,
            "control" => changed.control_source_revision += 1,
            "pause" => changed.run_intent = RunIntent::Pause,
            "owner" => {
                changed.workflow.coordinator = Some(CoordinatorLease {
                    id: "replacement".into(),
                    expires_at_ms: 100,
                })
            }
            "budget" => changed.budget.resume_required = true,
            _ => {
                let attempt = changed
                    .workflow
                    .completion
                    .attempt
                    .as_mut()
                    .ok_or(GoalError::NotFound)?;
                match mutation {
                    "recipe" => attempt.checks[0].spec.recipe = CheckRecipe::NpmTest,
                    "artifact" => attempt.checks[0].workspace.sha256 = "b".repeat(64),
                    "workspace" => attempt.workspace.push("elsewhere"),
                    _ => return Err(GoalError::Conflict),
                }
            }
        }
        assert!(authorize(&changed, "decision", 2).is_err(), "{mutation}");
    }
    apply(
        &mut goal,
        &Event::Result {
            decision_id: "decision".into(),
            operation_id: "explicit".into(),
            submitted: true,
            detail: "Consent consumed".into(),
        },
    )?;
    assert!(authorize(&goal, "decision", 2).is_err());
    Ok(())
}

#[test]
fn denial_cannot_authorize_a_command() -> Result<(), GoalError> {
    let (mut goal, decision) = pending();
    apply(
        &mut goal,
        &Event::Observe {
            decision: Box::new(decision),
        },
    )?;
    apply(
        &mut goal,
        &Event::Respond {
            decision_id: "decision".into(),
            operation_id: "deny".into(),
            option_id: "deny-once".into(),
            automatic: false,
        },
    )?;
    apply(
        &mut goal,
        &Event::RequestDelivery {
            decision_id: "decision".into(),
            operation_id: "deny".into(),
        },
    )?;
    assert!(authorize(&goal, "decision", 2).is_err());
    Ok(())
}
