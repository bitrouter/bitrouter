//! Real SQLite admission/link interruptions and an actual ACP startup fixture.
//! Native admission uses an unavailable model route; these cases do not qualify
//! provider execution, delivery completion or active cold recovery.

use super::*;
use bitrouter_orchestrator::agent::AgentConfig;
use bitrouter_orchestrator::goal::resources::Selection;
use bitrouter_orchestrator::service::ThreadService;
use bitrouter_orchestrator::store::ExecutionStore;
use bitrouter_orchestrator::thread::{
    PermissionProfile, ThreadRequest, ThreadStatus, ThreadTarget,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

async fn coordinator(home: &Path, database_url: &str) -> Result<(Coordinator, DatabaseConnection)> {
    coordinator_with_config(home, database_url, "").await
}

async fn coordinator_with_config(
    home: &Path,
    database_url: &str,
    extra: &str,
) -> Result<(Coordinator, DatabaseConnection)> {
    let configuration = format!(
        "inherit_defaults: false\nregistry: {{enabled: false}}\ndatabase: {{url: {}}}\nchat: {{model: later-configured-route}}\n{extra}",
        serde_json::to_string(database_url)?
    );
    let config_path = home.join("owner.yaml");
    tokio::fs::write(&config_path, &configuration).await?;
    let config = bitrouter_sdk::config::parse_with(&configuration, |_| None)?;
    let db = crate::db::connect(database_url).await?;
    crate::db::run_migrations(&db).await?;
    let app = Arc::new(App::builder().build()?);
    let threads = ThreadService::with_store(
        app.clone(),
        &[],
        Arc::new(crate::agent_store::DatabaseExecutionStore::new(db.clone())),
    )?;
    let goals = Arc::new(GoalService::new(Arc::new(
        crate::goal_store::DatabaseGoalStore::new(db.clone()),
    )));
    let routing = Arc::new(ConfigRoutingTable::from_config(config));
    let supervisor = Supervisor::open(
        crate::paths::ConfigSource::File(config_path),
        routing.clone(),
    )
    .await?;
    let SessionResponse::Authorized { grant } = supervisor
        .dispatch(SessionRequest::authorize(
            "owner-link-test".into(),
            BTreeSet::from([SessionScope::Peek]),
        ))
        .await?
    else {
        anyhow::bail!("owner test grant missing")
    };
    Ok((
        Coordinator {
            id: "owner-link-test".into(),
            app,
            goals,
            threads,
            supervisor,
            routing,
            workspaces: home.join("workspaces"),
            home: home.into(),
            grant,
            workspace_gate: tokio::sync::Mutex::new(()),
            stop_gate: tokio::sync::Mutex::new(()),
            gate: crate::daemon_handoff::HandoffGate::default(),
        },
        db,
    ))
}

async fn read(coordinator: &Coordinator, id: &str) -> Result<GoalSnapshot> {
    let GoalReply::Snapshot { goal } = coordinator
        .goals
        .execute("local", GoalCommand::Get { goal_id: id.into() }, now()?)
        .await?
    else {
        anyhow::bail!("owner test Goal missing")
    };
    Ok(*goal)
}

async fn prepared_native_attempt(original: &Coordinator, home: &Path) -> Result<String> {
    prepared_worker_attempt(original, home, None, Selection::default()).await
}

async fn prepared_worker_attempt(
    original: &Coordinator,
    home: &Path,
    harness: Option<&str>,
    selection: Selection,
) -> Result<String> {
    use bitrouter_orchestrator::goal::completion::{Condition, Specification};
    use bitrouter_orchestrator::goal::resources::Prepared;
    use bitrouter_orchestrator::goal::workflow::{Plan, TaskSpec};

    let GoalReply::Accepted { operation } = original
        .goals
        .execute(
            "local",
            GoalCommand::Create {
                operation_id: "create-native-admission".into(),
                title: "Native admission recovery".into(),
                workspace: home.into(),
                objective: "Retain the original accepted action".into(),
                budget_limits: Default::default(),
            },
            now()?,
        )
        .await?
    else {
        anyhow::bail!("creation receipt missing");
    };
    let id = operation.goal_id;
    let workspace = original.workspaces.join(&id).join("original-work");
    tokio::fs::create_dir_all(&workspace).await?;
    let goal = read(original, &id).await?;
    original.claim_owner(&goal).await?;
    original.owner_thread(&goal).await?;
    let prompt = crate::goal_artifacts::put(
        &crate::goal_artifacts::root(&original.workspaces, &id),
        if harness.is_some() {
            b"Assigned task left:\nSaved original ACP instruction"
        } else {
            b"Original saved worker input"
        },
    )
    .await?;
    let descriptors =
        super::super::resources::servers(&original.routing.snapshot_config(), &selection)?;
    let mcp_configuration_sha256 =
        super::super::resources::digest(&serde_json::to_vec(&descriptors)?);
    for change in [
        Change::RequestPlan {
            attempt_id: "plan".into(),
        },
        Change::InstallPlan {
            attempt_id: "plan".into(),
            plan: Plan {
                tasks: vec![TaskSpec {
                    id: "work".into(),
                    title: "Native work".into(),
                    instruction: "Saved instruction".into(),
                    harness: harness.map(str::to_owned),
                    dependencies: vec![],
                    checks: vec![],
                    resources: selection.clone(),
                }],
                completion: Specification {
                    conditions: vec![Condition {
                        id: "result".into(),
                        description: "Verified result".into(),
                    }],
                    checks: vec![],
                    human_acceptance_required: false,
                },
            },
            available_harnesses: harness.map(str::to_owned).into_iter().collect(),
        },
        Change::RequestExecution {
            task_id: "work".into(),
            execution_id: "original-work".into(),
            workspace: workspace.clone(),
        },
        Change::RegisterWorkspace {
            execution_id: "original-work".into(),
            base_commit: "a".repeat(40),
        },
        Change::PrepareResources {
            execution_id: "original-work".into(),
            resources: Prepared {
                worker_handoff: None,
                selection,
                prompt,
                materials: vec![],
                checked_dependencies: None,
                mcp_configuration_sha256,
            },
        },
        Change::RequestWorkerDispatch {
            execution_id: "original-work".into(),
        },
    ] {
        original.change(&read(original, &id).await?, change).await?;
    }
    Ok(id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_acp_intent_without_any_thread_admission_uses_its_original_action() -> Result<()> {
    let home = tempfile::tempdir()?;
    let controls = home.path().join("controls");
    let captures = home.path().join("captures");
    tokio::fs::create_dir_all(&controls).await?;
    tokio::fs::create_dir_all(&captures).await?;
    let extra = format!(
        "agents:\n  goal-peer:\n    name: goal-peer\n    transport:\n      type: stdio\n      command: {}\n      env:\n        BRO_GOAL_FIXTURE_CONTROL: {}\n        BRO_GOAL_FIXTURE_CAPTURE: {}\n",
        serde_json::to_string(&peer_executable()?)?,
        serde_json::to_string(&controls)?,
        serde_json::to_string(&captures)?
    );
    let (coordinator, db) = coordinator_with_config(home.path(), "sqlite::memory:", &extra).await?;
    let result = async {
        let id = prepared_worker_attempt(
            &coordinator,
            home.path(),
            Some("goal-peer"),
            Selection::default(),
        )
        .await?;
        let before = read(&coordinator, &id).await?;
        let task = &before.workflow.tasks[0];
        let original = &task.executions[0];
        let source = super::super::native::source(&before, task, original)?;
        let epoch = coordinator.threads.capabilities().server_instance_id;
        let caller = CallerContext::local();
        let key = bitrouter_orchestrator::service::worker::thread_key(&source)?;
        assert!(
            coordinator
                .threads
                .find_thread_admission(&epoch, &caller, &key)
                .await?
                .is_none()
        );
        coordinator.drive_task(&before, task).await?;
        let linked = read(&coordinator, &id).await?;
        let binding = linked.workflow.tasks[0].executions[0]
            .thread
            .clone()
            .context("saved new ACP dispatch did not enter common admission")?;
        let target = ThreadTarget {
            thread_id: binding.thread_id.clone(),
            server_instance_id: epoch.clone(),
        };
        let session = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let turn = coordinator
                    .threads
                    .read_stored_turn(&target, &caller, &binding.turn_id)
                    .await?;
                if let Some(session) = turn.acp {
                    return Ok::<_, anyhow::Error>(session);
                }
                anyhow::ensure!(
                    !turn.status.terminal(),
                    "ACP stopped before opening its original session: {:?}",
                    turn.detail
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        for _ in 0..2 {
            let goal = read(&coordinator, &id).await?;
            coordinator
                .drive_task(&goal, &goal.workflow.tasks[0])
                .await?;
        }
        let current = read(&coordinator, &id).await?;
        assert_eq!(current.workflow.tasks[0].executions.len(), 1);
        let attempt = &current.workflow.tasks[0].executions[0];
        assert_eq!(attempt.id, original.id);
        assert_eq!(attempt.thread.as_ref(), Some(&binding));
        assert_eq!(
            super::super::native::source(&current, &current.workflow.tasks[0], attempt)?,
            source
        );
        assert!(!current.workflow.completion.completed_for(1));
        let receipt = coordinator
            .threads
            .find_goal_worker_turn(
                &target,
                &caller,
                &super::super::native::turn_key(&attempt.id),
            )
            .await?
            .context("original accepted action missing")?;
        assert_eq!(receipt.turn_id, binding.turn_id);
        assert_eq!(
            coordinator
                .threads
                .find_thread_admission(&epoch, &caller, &key)
                .await?
                .context("original Thread receipt missing")?
                .thread_id,
            binding.thread_id
        );
        let owner = bitrouter_orchestrator::service::worker::acp::Owner {
            server_instance_id: epoch,
            thread_id: binding.thread_id,
            turn_id: binding.turn_id,
        };
        let (snapshot, _) = coordinator
            .supervisor
            .managed_observation(&owner, &session.run_id, 0)
            .await?;
        assert_eq!(
            snapshot.native_session_id.as_ref(),
            Some(&session.session_id)
        );
        assert_eq!(snapshot.managed_by.as_ref(), Some(&owner));
        let mut entries = tokio::fs::read_dir(&captures).await?;
        let mut initialize = 0;
        while let Some(entry) = entries.next_entry().await? {
            for line in tokio::fs::read_to_string(entry.path()).await?.lines() {
                let value: serde_json::Value = serde_json::from_str(line)?;
                initialize += usize::from(value["method"] == "initialize");
            }
        }
        assert_eq!(
            initialize, 1,
            "original dispatch retry started another harness"
        );
        Ok(())
    }
    .await;
    coordinator.threads.shutdown().await;
    coordinator.supervisor.shutdown().await;
    db.close().await?;
    result
}

async fn mcp_worker_configuration(home: &Path) -> Result<String> {
    for name in ["controls", "captures"] {
        tokio::fs::create_dir_all(home.join(name)).await?;
    }
    let mcp = fixture_binary::build("harness_mcp_fixture").map_err(anyhow::Error::msg)?;
    Ok(format!(
        "mcp_servers:\n  selected:\n    name: selected\n    transport:\n      type: stdio\n      command: {}\n      env: {{FIXTURE_SELECTED_VERSION: old}}\nagents:\n  goal-peer:\n    name: goal-peer\n    transport:\n      type: stdio\n      command: {}\n      env:\n        BRO_GOAL_FIXTURE_CONTROL: {}\n        BRO_GOAL_FIXTURE_CAPTURE: {}\n",
        serde_json::to_string(&mcp)?,
        serde_json::to_string(&peer_executable()?)?,
        serde_json::to_string(&home.join("controls"))?,
        serde_json::to_string(&home.join("captures"))?
    ))
}

async fn change_selected_mcp(coordinator: &Coordinator) -> Result<()> {
    let mut config = coordinator.routing.snapshot_config();
    let server = config
        .mcp_servers
        .get_mut("selected")
        .context("selected configuration missing")?;
    let bitrouter_sdk::mcp::transport::McpTransport::Stdio { env, .. } = &mut server.transport
    else {
        anyhow::bail!("fixture transport changed")
    };
    env.insert("FIXTURE_SELECTED_VERSION".into(), "new".into());
    coordinator.routing.replace_prepared_config(config).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_native_and_acp_dispatch_refuses_changed_selected_mcp_configuration() -> Result<()>
{
    for (harness, preaccepted_metadata) in [
        (None, false),
        (Some("goal-peer"), false),
        (Some("goal-peer"), true),
    ] {
        let home = tempfile::tempdir()?;
        let extra = mcp_worker_configuration(home.path()).await?;
        let (coordinator, db) =
            coordinator_with_config(home.path(), "sqlite::memory:", &extra).await?;
        let result: Result<()> = async {
            let selection = Selection { mcp_servers:vec!["selected".into()], ..Default::default() };
            let id = prepared_worker_attempt(&coordinator, home.path(), harness, selection).await?;
            let goal = read(&coordinator, &id).await?;
            let task = &goal.workflow.tasks[0];
            let execution = &task.executions[0];
            let prepared = execution.resources.as_ref().context("saved resources missing")?;
            let original = prepared.clone();
            let source = super::super::native::source(&goal, task, execution)?;
            let epoch = coordinator.threads.capabilities().server_instance_id;
            let key = bitrouter_orchestrator::service::worker::thread_key(&source)?;
            let accepted_thread = if preaccepted_metadata {
                Some(coordinator.threads.create_goal_acp_thread(
                    &ThreadTarget { thread_id:source.owner_thread_id.clone(), server_instance_id:epoch.clone() },
                    &source,
                    bitrouter_orchestrator::thread::executor::AcpThreadRequest {
                        caller:CallerContext::local(), workspace:coordinator.native_workspace(&goal, execution)?,
                        config:bitrouter_orchestrator::thread::executor::AcpConfig {
                            agent_id:"goal-peer".into(), requested_model_route:None, max_duration:Duration::from_secs(600),
                        }, permission_profile:PermissionProfile::Ask, idempotency_key:key.clone(),
                    },
                ).await?)
            } else { None };
            change_selected_mcp(&coordinator).await?;
            let descriptors = super::super::resources::servers(&coordinator.routing.snapshot_config(), &prepared.selection)?;
            assert_ne!(super::super::resources::digest(&serde_json::to_vec(&descriptors)?), prepared.mcp_configuration_sha256);
            let prompt = String::from_utf8(crate::goal_artifacts::read(&crate::goal_artifacts::root(&coordinator.workspaces, &id), &prepared.prompt).await?)?;
            let launched = match harness {
                Some(harness) => Box::pin(coordinator.launch_acp(&goal, task, &execution.id, prompt, harness, descriptors)).await,
                None => Box::pin(coordinator.launch_native(&goal, task, &execution.id, prompt)).await,
            };
            anyhow::ensure!(launched.as_ref().err().is_some_and(|error| error.to_string().contains("selected MCP configuration changed")),
                "changed frozen MCP configuration was not fenced before {harness:?} admission: {launched:?}");
            let retained_thread = coordinator.threads.find_thread_admission(&epoch, &CallerContext::local(), &key).await?;
            assert_eq!(retained_thread.as_ref().map(|thread| &thread.thread_id), accepted_thread.as_ref().map(|thread| &thread.thread_id));
            if let Some(thread) = retained_thread {
                assert!(coordinator.threads.find_goal_worker_turn(&ThreadTarget { thread_id:thread.thread_id, server_instance_id:epoch },
                    &CallerContext::local(), &super::super::native::turn_key(&execution.id)).await?.is_none());
            }
            assert!(!execution.workspace.join("mcp.pid").exists());
            assert!(tokio::fs::read_dir(home.path().join("captures")).await?.next_entry().await?.is_none());
            let retained = read(&coordinator, &id).await?;
            assert_eq!(retained.workflow.tasks[0].executions[0].resources.as_ref(), Some(&original));
            Ok(())
        }.await;
        coordinator.threads.shutdown().await;
        coordinator.supervisor.shutdown().await;
        db.close().await?;
        result?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_dispatch_without_a_turn_can_prepare_again_without_erasing_its_intent() -> Result<()>
{
    for (harness, preaccepted_metadata, corrupt_prompt) in [
        (None, false, false),
        (Some("goal-peer"), false, false),
        (Some("goal-peer"), true, false),
        (None, false, true),
    ] {
        let home = tempfile::tempdir()?;
        let extra = mcp_worker_configuration(home.path()).await?;
        let (coordinator, db) =
            coordinator_with_config(home.path(), "sqlite::memory:", &extra).await?;
        let result: Result<()> = async {
            let selection = Selection {
                mcp_servers: vec!["selected".into()],
                ..Default::default()
            };
            let id = prepared_worker_attempt(&coordinator, home.path(), harness, selection).await?;
            let before = read(&coordinator, &id).await?;
            let task = &before.workflow.tasks[0];
            let original = task.executions[0].clone();
            if preaccepted_metadata {
                let source = super::super::native::source(&before, task, &original)?;
                coordinator
                    .threads
                    .create_goal_acp_thread(
                        &ThreadTarget {
                            thread_id: source.owner_thread_id.clone(),
                            server_instance_id: coordinator
                                .threads
                                .capabilities()
                                .server_instance_id,
                        },
                        &source,
                        bitrouter_orchestrator::thread::executor::AcpThreadRequest {
                            caller: CallerContext::local(),
                            workspace: coordinator.native_workspace(&before, &original)?,
                            config: bitrouter_orchestrator::thread::executor::AcpConfig {
                                agent_id: "goal-peer".into(),
                                requested_model_route: None,
                                max_duration: Duration::from_secs(600),
                            },
                            permission_profile: PermissionProfile::Ask,
                            idempotency_key: bitrouter_orchestrator::service::worker::thread_key(
                                &source,
                            )?,
                        },
                    )
                    .await?;
            }
            if corrupt_prompt {
                let prompt = &original.resources.as_ref().context("saved input missing")?.prompt;
                tokio::fs::write(&prompt.path, vec![b'x'; usize::try_from(prompt.bytes)?]).await?;
            } else {
                change_selected_mcp(&coordinator).await?;
            }
            // The real launcher saves uncertainty first. Only a separate
            // original-action reconciliation may classify it as not admitted.
            coordinator.drive_task(&before, task).await.context("pre-admission refusal was not durably recorded")?;
            let pending = read(&coordinator, &id).await?;
            if corrupt_prompt {
                assert!(matches!(&pending.workflow.tasks[0].executions[0].observation,
                    Observation::Unknown { reason, .. } if reason.contains("artifact content changed")));
            }
            assert!(matches!(
                pending.workflow.tasks[0].executions[0].observation,
                Observation::Unknown { run_id: None, .. }
            ));
            coordinator
                .drive_task(&pending, &pending.workflow.tasks[0])
                .await?;
            let reconciled = read(&coordinator, &id).await?;
            let task = &reconciled.workflow.tasks[0];
            assert_eq!(
                task.execution_admission(
                    reconciled.requirements_revision,
                    reconciled.control_source_revision
                ),
                ExecutionAdmission::Ready,
                "an original action with no accepted Turn remains permanently blocked"
            );
            assert!(
                task.executions[0].dispatch_requested,
                "the saved dispatch intent must remain in history"
            );
            assert_eq!(task.executions[0].resources, original.resources);
            assert!(task.executions[0].thread.is_none());
            assert!(matches!(
                task.executions[0].observation,
                Observation::NotAdmitted { .. }
            ));
            // This verdict is terminal for the attempt, including late links.
            assert!(
                coordinator
                    .change(
                        &reconciled,
                        Change::BindWorkerTurn {
                            execution_id: original.id.clone(),
                            thread: bitrouter_orchestrator::goal::workflow::ThreadExecution {
                                thread_id: "late-thread".into(),
                                turn_id: "late-turn".into(),
                            },
                        }
                    )
                    .await
                    .is_err()
            );
            assert!(!original.workspace.join("mcp.pid").exists());
            assert!(
                tokio::fs::read_dir(home.path().join("captures"))
                    .await?
                    .next_entry()
                    .await?
                    .is_none()
            );
            coordinator
                .change(
                    &reconciled,
                    Change::RequestExecution {
                        task_id: task.spec.id.clone(),
                        execution_id: "prepared-again".into(),
                        workspace: coordinator.workspaces.join(&id).join("prepared-again"),
                    },
                )
                .await?;
            let next = read(&coordinator, &id).await?;
            assert_eq!(next.workflow.tasks[0].executions.len(), 2);
            assert_eq!(next.workflow.tasks[0].executions[0], task.executions[0]);
            assert_eq!(
                next.workflow.owner_thread_id,
                reconciled.workflow.owner_thread_id
            );
            Ok(())
        }
        .await;
        coordinator.threads.shutdown().await;
        coordinator.supervisor.shutdown().await;
        db.close().await?;
        result?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_dispatch_with_a_lost_goal_link_retains_the_original_accepted_turn() -> Result<()> {
    for harness in [None, Some("goal-peer")] {
        let home = tempfile::tempdir()?;
        let extra = mcp_worker_configuration(home.path()).await?;
        let (coordinator, db) =
            coordinator_with_config(home.path(), "sqlite::memory:", &extra).await?;
        let result: Result<()> = async {
            let id = prepared_worker_attempt(&coordinator, home.path(), harness, Selection::default()).await?;
            let before = read(&coordinator, &id).await?;
            let task = &before.workflow.tasks[0];
            let execution = &task.executions[0];
            let source = super::super::native::source(&before, task, execution)?;
            let epoch = coordinator.threads.capabilities().server_instance_id;
            let prompt = String::from_utf8(crate::goal_artifacts::read(
                &crate::goal_artifacts::root(&coordinator.workspaces, &id),
                &execution.resources.as_ref().context("prepared input missing")?.prompt,
            ).await?)?;
            db.execute(Statement::from_string(DatabaseBackend::Sqlite,
                "CREATE TRIGGER reject_work_link BEFORE UPDATE ON bro_goals WHEN json_extract(CAST(NEW.payload AS TEXT), '$.workflow.tasks[0].executions[0].thread') IS NOT NULL BEGIN SELECT RAISE(ABORT, 'worker link interrupted'); END;"
            )).await?;
            let result = match harness {
                Some(harness) => Box::pin(coordinator.launch_acp(&before, task, &execution.id, prompt, harness, vec![])).await,
                None => Box::pin(coordinator.launch_native(&before, task, &execution.id, prompt)).await,
            };
            assert!(result.is_err());
            db.execute(Statement::from_string(DatabaseBackend::Sqlite, "DROP TRIGGER reject_work_link")).await?;
            let receipt = coordinator.threads.find_goal_worker_admission(&epoch, &CallerContext::local(), &source,
                &super::super::native::turn_key(&execution.id)).await?.context("the original Turn was not accepted")?;
            let current = read(&coordinator, &id).await?;
            assert!(current.workflow.tasks[0].executions[0].thread.is_none());
            coordinator.change(&current, Change::ObserveExecution {
                execution_id:execution.id.clone(), observation:Observation::Unknown {run_id:None, reason:"Acceptance succeeded but the Goal link was interrupted".into()},
            }).await?;
            let uncertain = read(&coordinator, &id).await?;
            coordinator.drive_task(&uncertain, &uncertain.workflow.tasks[0]).await?;
            let linked = read(&coordinator, &id).await?;
            let retained = &linked.workflow.tasks[0].executions[0];
            assert_eq!(retained.thread, Some(bitrouter_orchestrator::goal::workflow::ThreadExecution {
                thread_id:receipt.thread_id.clone(), turn_id:receipt.turn_id.clone(),
            }));
            assert!(matches!(retained.observation, Observation::Unknown { .. }));
            assert_eq!(linked.workflow.tasks[0].executions.len(), 1);
            assert_eq!(linked.workflow.tasks[0].execution_admission(linked.requirements_revision, linked.control_source_revision), ExecutionAdmission::Blocked);
            assert!(coordinator.change(&linked, Change::RejectWorkerDispatch { execution_id:execution.id.clone(), reason:"A missing binding is not no-admission evidence".into() }).await.is_err());
            assert_eq!(coordinator.threads.find_goal_worker_admission(&epoch, &CallerContext::local(), &source,
                &super::super::native::turn_key(&execution.id)).await?.map(|receipt|receipt.turn_id), Some(receipt.turn_id));
            Ok(())
        }.await;
        coordinator.threads.shutdown().await;
        coordinator.supervisor.shutdown().await;
        db.close().await?;
        result?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_native_and_acp_receipts_keep_original_resources_after_mcp_config_change()
-> Result<()> {
    for harness in [None, Some("goal-peer")] {
        let home = tempfile::tempdir()?;
        let extra = mcp_worker_configuration(home.path()).await?;
        let (coordinator, db) =
            coordinator_with_config(home.path(), "sqlite::memory:", &extra).await?;
        let result: Result<()> = async {
            let selection = Selection {
                mcp_servers: vec!["selected".into()],
                ..Default::default()
            };
            let id = prepared_worker_attempt(&coordinator, home.path(), harness, selection).await?;
            let before = read(&coordinator, &id).await?;
            let original_resources = before.workflow.tasks[0].executions[0]
                .resources
                .clone()
                .context("prepared resources missing")?;
            let prompt = String::from_utf8(
                crate::goal_artifacts::read(
                    &crate::goal_artifacts::root(&coordinator.workspaces, &id),
                    &original_resources.prompt,
                )
                .await?,
            )?;
            let descriptors = super::super::resources::servers(
                &coordinator.routing.snapshot_config(),
                &original_resources.selection,
            )?;
            let task = &before.workflow.tasks[0];
            match harness {
                Some(harness) => {
                    Box::pin(coordinator.launch_acp(
                        &before,
                        task,
                        "original-work",
                        prompt.clone(),
                        harness,
                        descriptors,
                    ))
                    .await?
                }
                None => {
                    Box::pin(coordinator.launch_native(
                        &before,
                        task,
                        "original-work",
                        prompt.clone(),
                    ))
                    .await?
                }
            }
            let accepted = read(&coordinator, &id).await?;
            let binding = accepted.workflow.tasks[0].executions[0]
                .thread
                .clone()
                .context("original binding missing")?;
            change_selected_mcp(&coordinator).await?;
            let descriptors = super::super::resources::servers(
                &coordinator.routing.snapshot_config(),
                &original_resources.selection,
            )?;
            assert_ne!(
                super::super::resources::digest(&serde_json::to_vec(&descriptors)?),
                original_resources.mcp_configuration_sha256
            );
            let task = &accepted.workflow.tasks[0];
            match harness {
                Some(harness) => {
                    Box::pin(coordinator.launch_acp(
                        &accepted,
                        task,
                        "original-work",
                        prompt,
                        harness,
                        descriptors,
                    ))
                    .await?
                }
                None => {
                    Box::pin(coordinator.launch_native(&accepted, task, "original-work", prompt))
                        .await?
                }
            }
            let replayed = read(&coordinator, &id).await?;
            assert_eq!(replayed.workflow.tasks[0].executions.len(), 1);
            let retained = &replayed.workflow.tasks[0].executions[0];
            assert_eq!(retained.thread.as_ref(), Some(&binding));
            assert_eq!(retained.resources.as_ref(), Some(&original_resources));
            assert!(!replayed.workflow.completion.completed_for(1));
            Ok(())
        }
        .await;
        coordinator.threads.shutdown().await;
        coordinator.supervisor.shutdown().await;
        db.close().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn native_late_stop_evidence_uses_the_original_turn_even_after_resume() -> Result<()> {
    for (unknown_observation, resumed, failed_commit) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let home = tempfile::tempdir()?;
        let (coordinator, db) = coordinator(home.path(), "sqlite::memory:").await?;
        let result: Result<()> = async {
            let id = prepared_native_attempt(&coordinator, home.path()).await?;
            if failed_commit {
                db.execute(Statement::from_string(DatabaseBackend::Sqlite,
                    "CREATE TRIGGER reject_native_stop_fact BEFORE INSERT ON bro_execution_records WHEN json_extract(NEW.payload, '$.record') = 'turn_lifecycle' AND json_extract(NEW.payload, '$.lifecycle.kind') = 'finished' BEGIN SELECT RAISE(ABORT, 'native terminal commit interrupted'); END;"
                )).await?;
            }
            let goal = read(&coordinator, &id).await?;
            coordinator
                .launch_native(
                    &goal,
                    &goal.workflow.tasks[0],
                    "original-work",
                    "Original saved worker input".into(),
                )
                .await?;
            let admitted = read(&coordinator, &id).await?;
            let binding = admitted.workflow.tasks[0].executions[0]
                .thread
                .clone()
                .context("original native Turn was not bound")?;
            let target = ThreadTarget {
                thread_id: binding.thread_id.clone(),
                server_instance_id: coordinator.threads.capabilities().server_instance_id,
            };
            let caller = CallerContext::local();
            let terminal = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let turn = coordinator
                        .threads
                        .read_stored_turn(&target, &caller, &binding.turn_id)
                        .await?;
                    if turn.status.terminal() || turn.status == TurnStatus::RecoveryRequired {
                        return Ok::<_, anyhow::Error>(turn);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await??;
            assert_eq!(terminal.status, if failed_commit { TurnStatus::RecoveryRequired } else { TurnStatus::Failed });
            assert_eq!(terminal.unknown_effect, failed_commit);
            let current = read(&coordinator, &id).await?;
            coordinator
                .goals
                .execute(
                    "local",
                    GoalCommand::Control {
                        operation_id: "pause-original-work".into(),
                        goal_id: id.clone(),
                        expected_revision: current.revision,
                        intent: RunIntent::Pause,
                    },
                    now()?,
                )
                .await?;
            if unknown_observation {
                coordinator
                    .change(
                        &read(&coordinator, &id).await?,
                        Change::ObserveExecution {
                            execution_id: "original-work".into(),
                            observation: Observation::Unknown {
                                run_id: None,
                                reason: "Original native outcome was not projected".into(),
                            },
                        },
                    )
                    .await?;
            }
            coordinator.change(&read(&coordinator, &id).await?,
                Change::RequestStop {
                    execution_id: "original-work".into(),
                    operation_id: "original-stop".into(),
                }
            ).await?;
            if !failed_commit {
                coordinator.change(&read(&coordinator, &id).await?,
                Change::ObserveStop {
                    execution_id: "original-work".into(),
                    operation_id: "original-stop".into(),
                    confirmed: false,
                    detail: "Original stop observation was unavailable".into(),
                }).await?;
            }
            if resumed {
                let paused = read(&coordinator, &id).await?;
                coordinator
                    .goals
                    .execute(
                        "local",
                        GoalCommand::Control {
                            operation_id: "resume-with-unknown-stop".into(),
                            goal_id: id.clone(),
                            expected_revision: paused.revision,
                            intent: RunIntent::Run,
                        },
                        now()?,
                    )
                    .await?;
            }
            let uncertain = read(&coordinator, &id).await?;
            coordinator.drive_controls(&uncertain).await?;
            let stopped = read(&coordinator, &id).await?;
            let original = &stopped.workflow.tasks[0].executions[0];
            match &original.stop {
                Some(StopState::Unknown { operation_id, .. }) if failed_commit => assert_eq!(operation_id, "original-stop"),
                Some(StopState::Confirmed { operation_id, .. }) if !failed_commit => assert_eq!(operation_id, "original-stop"),
                other => anyhow::bail!("incorrect stop evidence: {other:?}"),
            }
            assert_eq!(original.thread.as_ref(), Some(&binding));
            assert_eq!(
                original.observation,
                uncertain.workflow.tasks[0].executions[0].observation
            );
            assert!(!original.effects_reconciled());
            assert_eq!(stopped.workflow.tasks[0].executions.len(), 1);
            assert_eq!(stopped.run_intent, uncertain.run_intent);
            assert_eq!(
                stopped.control_source_revision,
                uncertain.control_source_revision
            );
            assert!(!stopped.workflow.completion.completed_for(1));
            assert_eq!(
                coordinator
                    .threads
                    .find_goal_worker_turn(
                        &target,
                        &caller,
                        &super::super::native::turn_key("original-work")
                    )
                    .await?
                    .context("original native receipt disappeared")?
                    .turn_id,
                binding.turn_id
            );
            coordinator.drive_controls(&stopped).await?;
            assert_eq!(read(&coordinator, &id).await?.revision, stopped.revision);
            Ok(())
        }
        .await;
        if failed_commit {
            db.execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "DROP TRIGGER IF EXISTS reject_native_stop_fact;",
            ))
            .await?;
        }
        coordinator.threads.shutdown().await;
        coordinator.supervisor.shutdown().await;
        drop(coordinator);
        db.close().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn native_dispatch_before_turn_acceptance_uses_original_input_and_keys_after_reopen()
-> Result<()> {
    for (accepted_thread, paused) in [(false, false), (true, false), (false, true)] {
        let home = tempfile::tempdir()?;
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            home.path().join("state.db").display()
        );
        let (original, db) = coordinator(home.path(), &database_url).await?;
        let id = prepared_native_attempt(&original, home.path()).await?;
        let workspace = original.workspaces.join(&id).join("original-work");
        let goal = read(&original, &id).await?;
        let task = &goal.workflow.tasks[0];
        let owner = original.owner_thread(&goal).await?;
        let source = super::super::native::source(&goal, task, &task.executions[0])?;
        let key = bitrouter_orchestrator::service::worker::thread_key(&source)?;
        let original_thread = if accepted_thread {
            let workspace = original.threads.register_local_workspace(&workspace)?;
            Some(
                original
                    .threads
                    .create_goal_worker_thread(
                        &ThreadTarget {
                            thread_id: owner.thread_id,
                            server_instance_id: original.threads.capabilities().server_instance_id,
                        },
                        &source,
                        ThreadRequest {
                            caller: CallerContext::local(),
                            workspace,
                            config: AgentConfig::fixed("later-configured-route", None),
                            permission_profile: PermissionProfile::Ask,
                            verification_command: None,
                            idempotency_key: key.clone(),
                        },
                        None,
                    )
                    .await?
                    .thread_id,
            )
        } else {
            None
        };
        if paused {
            original
                .goals
                .execute(
                    "local",
                    GoalCommand::Control {
                        operation_id: "pause-before-admission".into(),
                        goal_id: id.clone(),
                        expected_revision: goal.revision,
                        intent: RunIntent::Pause,
                    },
                    now()?,
                )
                .await?;
        }
        original.threads.shutdown().await;
        original.supervisor.shutdown().await;
        drop(original);
        db.close().await?;

        let (reopened, db) = coordinator(home.path(), &database_url).await?;
        let result: Result<()> = async {
            let goal = read(&reopened, &id).await?;
            reopened.claim_owner(&goal).await?;
            let task = &goal.workflow.tasks[0];
            let mut outside = task.executions[0].clone();
            outside.workspace = home.path().into();
            assert!(reopened.drive_worker(&goal, task, &outside).await.is_err());
            #[cfg(unix)]
            if !accepted_thread && !paused {
                let moved = home.path().join("outside-worker");
                tokio::fs::rename(&workspace, &moved).await?;
                std::os::unix::fs::symlink(&moved, &workspace)?;
                assert!(
                    reopened
                        .drive_worker(&goal, task, &task.executions[0])
                        .await
                        .is_err()
                );
                tokio::fs::remove_file(&workspace).await?;
                tokio::fs::rename(&moved, &workspace).await?;
            }
            reopened
                .drive_worker(&goal, task, &task.executions[0])
                .await?;
            let current = read(&reopened, &id).await?;
            assert_eq!(current.workflow.tasks[0].executions.len(), 1);
            assert!(!current.workflow.completion.completed_for(1));
            let epoch = reopened.threads.capabilities().server_instance_id;
            let caller = CallerContext::local();
            let thread = reopened
                .threads
                .find_thread_admission(&epoch, &caller, &key)
                .await?;
            if paused {
                assert!(thread.is_none());
                assert!(current.workflow.tasks[0].executions[0].thread.is_none());
                assert_eq!(current.run_intent, RunIntent::Pause);
                return Ok(());
            }
            let thread = thread.context("original worker admission missing")?;
            if let Some(original_thread) = original_thread {
                assert_eq!(thread.thread_id, original_thread);
            }
            let binding = current.workflow.tasks[0].executions[0]
                .thread
                .as_ref()
                .context("worker link missing")?;
            assert_eq!(binding.thread_id, thread.thread_id);
            let target = ThreadTarget {
                thread_id: thread.thread_id,
                server_instance_id: epoch,
            };
            let receipt = reopened
                .threads
                .find_goal_worker_turn(
                    &target,
                    &caller,
                    &super::super::native::turn_key("original-work"),
                )
                .await?
                .context("original Turn missing")?;
            assert_eq!(receipt.turn_id, binding.turn_id);
            let records = crate::agent_store::DatabaseExecutionStore::new(db.clone())
                .read_records(&target.thread_id, 0, None, 32, 1024 * 1024)
                .await
                .map_err(anyhow::Error::msg)?
                .context("accepted worker history missing")?;
            assert!(records.records.iter().any(|record| matches!(record,
                bitrouter_orchestrator::store::ExecutionRecord::TurnQueued { turn_id, prompt, .. }
                    if turn_id == &receipt.turn_id && prompt == "Original saved worker input")));
            // A stale coordinator snapshot cannot admit another action.
            reopened
                .drive_worker(&goal, task, &task.executions[0])
                .await?;
            let repeated = reopened
                .threads
                .find_goal_worker_turn(
                    &target,
                    &caller,
                    &super::super::native::turn_key("original-work"),
                )
                .await?
                .context("repeated receipt missing")?;
            assert_eq!(repeated.turn_id, receipt.turn_id);
            Ok(())
        }
        .await;
        reopened.threads.shutdown().await;
        reopened.supervisor.shutdown().await;
        drop(reopened);
        db.close().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn interrupted_owner_link_reconciles_the_original_thread_after_database_reopen() -> Result<()>
{
    let home = tempfile::tempdir()?;
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        home.path().join("state.db").display()
    );
    let (original, db) = coordinator(home.path(), &database_url).await?;
    let GoalReply::Accepted { operation } = original
        .goals
        .execute(
            "local",
            GoalCommand::Create {
                operation_id: "create-owner-goal".into(),
                title: "Owner admission".into(),
                workspace: home.path().into(),
                objective: "Preserve the original accepted identity".into(),
                budget_limits: Default::default(),
            },
            now()?,
        )
        .await?
    else {
        anyhow::bail!("Goal creation missing")
    };
    let id = operation.goal_id;
    let goal = read(&original, &id).await?;
    original.claim_owner(&goal).await?;
    let workspace = original.threads.register_local_workspace(home.path())?;
    let key = bitrouter_orchestrator::goal::workflow::owner_thread_key(&id);
    let accepted = original
        .threads
        .create_goal_owner_thread(
            &original.threads.capabilities().server_instance_id,
            &id,
            ThreadRequest {
                caller: CallerContext::local(),
                workspace,
                config: AgentConfig::fixed("original-route", None).read_only(),
                permission_profile: PermissionProfile::ReadOnly,
                verification_command: None,
                idempotency_key: key,
            },
        )
        .await?;
    // The execution store accepted the Thread, but Goal linking cannot commit.
    db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "CREATE TRIGGER reject_owner_link BEFORE UPDATE ON bro_goals WHEN json_extract(CAST(NEW.payload AS TEXT), '$.workflow.owner_thread_id') IS NOT NULL BEGIN SELECT RAISE(ABORT, 'owner link interrupted'); END;"
    )).await?;
    assert!(original.owner_thread(&goal).await.is_err());
    assert!(
        read(&original, &id)
            .await?
            .workflow
            .owner_thread_id
            .is_none()
    );
    original.threads.shutdown().await;
    original.supervisor.shutdown().await;
    drop(original);
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "DROP TRIGGER reject_owner_link;",
    ))
    .await?;
    db.close().await?;

    let (reopened, db) = coordinator(home.path(), &database_url).await?;
    let result = async {
        let goal = read(&reopened, &id).await?;
        reopened.claim_owner(&goal).await?;
        let linked = reopened.owner_thread(&goal).await?;
        assert_eq!(linked.thread_id, accepted.thread_id);
        assert_eq!(linked.model, "original-route");
        assert_eq!(linked.owned_goal_id.as_deref(), Some(id.as_str()));
        assert!(linked.active_turn_id.is_none());
        let current = read(&reopened, &id).await?;
        assert_eq!(
            current.workflow.owner_thread_id.as_deref(),
            Some(accepted.thread_id.as_str())
        );
        assert_eq!(current.requirements_revision, goal.requirements_revision);
        assert_eq!(current.run_intent, goal.run_intent);
        assert_eq!(current.accounting, goal.accounting);
        let stored = crate::agent_store::DatabaseExecutionStore::new(db.clone());
        let page = stored
            .read_index(0, None, 16, 1024 * 1024)
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(page.entries.len(), 1);
        let view = reopened
            .threads
            .read_stored_thread_view(
                &ThreadTarget {
                    thread_id: linked.thread_id.clone(),
                    server_instance_id: reopened.threads.capabilities().server_instance_id,
                },
                &CallerContext::local(),
            )
            .await?;
        assert!(view.latest_turn.is_none());
        assert!(matches!(
            view.thread.status,
            ThreadStatus::Idle | ThreadStatus::RecoveryRequired
        ));
        assert_eq!(
            reopened.owner_thread(&current).await?.thread_id,
            accepted.thread_id
        );
        assert!(
            reopened
                .change(
                    &current,
                    Change::BindOwnerThread {
                        thread_id: "different-owner".into()
                    }
                )
                .await
                .is_err()
        );
        let legacy: GoalSnapshot = {
            let mut value = serde_json::to_value(&current)?;
            value["workflow"]
                .as_object_mut()
                .context("workflow missing")?
                .remove("owner_thread_id");
            serde_json::from_value(value)?
        };
        assert!(legacy.workflow.owner_thread_id.is_none());
        Ok(())
    }
    .await;
    reopened.threads.shutdown().await;
    reopened.supervisor.shutdown().await;
    result
}
