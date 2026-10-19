use super::*;
use bitrouter_orchestrator::goal::resources::{Material, Prepared, Selection};

#[tokio::test]
async fn selected_resources_must_be_frozen_before_dispatch_and_survive_reload() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let dir = tempfile::tempdir()?;
    let id = accepted(
        goals
            .execute("local", create("create-resource-goal", dir.path()), 10)
            .await?,
    )?
    .goal_id;
    claim(&goals, &id).await?;
    let mut spec = task("selected", &[]);
    spec.resources = Selection {
        model_route: Some("selected-route".into()),
        mcp_servers: vec!["configured-server".into()],
        context_files: vec!["seed.txt".into()],
        ..Default::default()
    };
    goals
        .workflow_change(
            actor(),
            &id,
            "plan-resource-request",
            1,
            Change::RequestPlan {
                attempt_id: "resource-plan".into(),
            },
            20,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "install-resource-plan",
            1,
            Change::InstallPlan {
                attempt_id: "resource-plan".into(),
                plan: Plan {
                    tasks: vec![spec.clone()],
                    completion: completion_spec(),
                },
                available_harnesses: BTreeSet::from(["configured-acp".into()]),
            },
            30,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "request-resource-execution",
            1,
            Change::RequestExecution {
                task_id: spec.id.clone(),
                execution_id: "resource-execution".into(),
                workspace: std::env::temp_dir().join("resource-execution"),
            },
            40,
        )
        .await?;
    assert!(
        goals
            .workflow_change(
                actor(),
                &id,
                "unprepared-dispatch",
                1,
                Change::RequestDispatch {
                    execution_id: "resource-execution".into()
                },
                41
            )
            .await
            .is_err()
    );
    goals
        .workflow_change(
            actor(),
            &id,
            "register-resource-workspace",
            1,
            Change::RegisterWorkspace {
                execution_id: "resource-execution".into(),
                base_commit: "a".repeat(40),
            },
            42,
        )
        .await?;
    let content = report(dir.path(), "selected context bytes")?;
    let prepared = Prepared {
        selection: spec.resources.clone(),
        prompt: report(dir.path(), "actual prepared prompt")?,
        materials: vec![Material::Context {
            path: "seed.txt".into(),
            content,
        }],
        checked_dependencies: None,
        mcp_configuration_sha256: "b".repeat(64),
    };
    let changed = Change::PrepareResources {
        execution_id: "resource-execution".into(),
        resources: prepared.clone(),
    };
    let receipt = goals
        .workflow_change(
            actor(),
            &id,
            "prepare-resource-input",
            1,
            changed.clone(),
            43,
        )
        .await?;
    assert_eq!(
        goals
            .workflow_change(actor(), &id, "prepare-resource-input", 1, changed, 44)
            .await?,
        receipt
    );
    goals
        .workflow_change(
            actor(),
            &id,
            "resource-dispatch",
            1,
            Change::RequestDispatch {
                execution_id: "resource-execution".into(),
            },
            45,
        )
        .await?;
    assert!(
        goals
            .workflow_change(
                actor(),
                &id,
                "route-without-run",
                1,
                Change::ConfirmModelRoute {
                    execution_id: "resource-execution".into(),
                    run_id: "resource-run".into(),
                    route: "selected-route".into()
                },
                46
            )
            .await
            .is_err()
    );
    goals
        .workflow_change(
            actor(),
            &id,
            "resource-run-mapped",
            1,
            Change::ObserveExecution {
                execution_id: "resource-execution".into(),
                observation: Observation::Running {
                    run_id: Some("resource-run".into()),
                    native_session_id: None,
                },
            },
            47,
        )
        .await?;
    for (operation, run, route) in [
        ("wrong-route-run", "other-run", "selected-route"),
        ("wrong-route-selector", "resource-run", "invented-route"),
    ] {
        assert!(
            goals
                .workflow_change(
                    actor(),
                    &id,
                    operation,
                    1,
                    Change::ConfirmModelRoute {
                        execution_id: "resource-execution".into(),
                        run_id: run.into(),
                        route: route.into()
                    },
                    48
                )
                .await
                .is_err()
        );
    }
    goals
        .workflow_change(
            actor(),
            &id,
            "confirmed-resource-route",
            1,
            Change::ConfirmModelRoute {
                execution_id: "resource-execution".into(),
                run_id: "resource-run".into(),
                route: "selected-route".into(),
            },
            49,
        )
        .await?;
    let reloaded = service(db);
    let current = snapshot(&reloaded, &id).await?;
    assert_eq!(
        current.workflow.tasks[0].executions[0].resources.as_ref(),
        Some(&prepared)
    );
    assert_eq!(
        current.workflow.tasks[0].executions[0]
            .confirmed_model_route
            .as_deref(),
        Some("selected-route")
    );
    assert!(
        goals
            .workflow_change(
                actor(),
                &id,
                "late-resource-overwrite",
                1,
                Change::PrepareResources {
                    execution_id: "resource-execution".into(),
                    resources: prepared
                },
                46
            )
            .await
            .is_err()
    );
    Ok(())
}

#[test]
fn resource_material_cannot_expand_selection_or_omit_the_selected_bytes() -> Result<()> {
    for path in ["../escape", "/absolute", "a\\b"] {
        assert!(
            Selection {
                context_files: vec![path.into()],
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
    assert!(
        Selection {
            mcp_servers: vec!["same".into(), "same".into()],
            ..Default::default()
        }
        .validate()
        .is_err()
    );
    let dir = tempfile::tempdir()?;
    let mut prepared = Prepared {
        selection: Selection {
            context_files: vec!["chosen".into()],
            ..Default::default()
        },
        prompt: report(dir.path(), "actual prompt")?,
        materials: vec![],
        checked_dependencies: None,
        mcp_configuration_sha256: "a".repeat(64),
    };
    assert!(prepared.validate().is_err());
    prepared.materials.push(Material::Context {
        path: "unselected".into(),
        content: report(dir.path(), "content")?,
    });
    assert!(prepared.validate().is_err());
    prepared.materials[0] = Material::Context {
        path: "chosen".into(),
        content: report(dir.path(), "selected")?,
    };
    prepared.checked_dependencies = Some(report(dir.path(), "actual checked dependency evidence")?);
    assert!(prepared.validate().is_ok());
    if let Some(reference) = &mut prepared.checked_dependencies {
        reference.bytes = 128 * 1024 + 1;
    }
    assert!(prepared.validate().is_err());
    Ok(())
}

#[test]
fn legacy_task_serialization_does_not_change_empty_resource_receipt_payloads() -> Result<()> {
    let legacy = serde_json::json!({"id":"old-task","title":"Original title","instruction":"Original instruction",
        "harness":"configured-acp","dependencies":[],"checks":[]});
    let decoded: TaskSpec = serde_json::from_value(legacy.clone())?;
    assert!(decoded.resources.is_empty());
    assert_eq!(serde_json::to_value(decoded)?, legacy);
    Ok(())
}
