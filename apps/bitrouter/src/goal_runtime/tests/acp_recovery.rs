//! Never-started ACP Thread metadata is not a restored private harness session.
use super::*;
use bitrouter_orchestrator::thread::executor::{AcpConfig, AcpThreadRequest};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unopened_acp_metadata_keeps_its_thread_after_confirmed_shutdown() -> Result<()> {
    let mut home = tempfile::tempdir()?;
    let keep = std::env::var_os("BRO_GOAL_FIXTURE_KEEP_FAILURES").is_some();
    home.disable_cleanup(keep);
    if keep {
        eprintln!(
            "Failure-retention ACP metadata fixture: {}",
            home.path().display()
        );
    }
    let extra = mcp_worker_configuration(home.path()).await?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        home.path().join("metadata.sqlite").display()
    );
    let (original, old_db) = coordinator_with_config(home.path(), &url, &extra).await?;
    let id = prepared_worker_attempt(
        &original,
        home.path(),
        Some("goal-peer"),
        Selection::default(),
    )
    .await?;
    let before = read(&original, &id).await?;
    let source = super::super::super::native::source(
        &before,
        &before.workflow.tasks[0],
        &before.workflow.tasks[0].executions[0],
    )?;
    let key = bitrouter_orchestrator::service::worker::thread_key(&source)?;
    let caller = CallerContext::local();
    let worker = original
        .threads
        .create_goal_acp_thread(
            &ThreadTarget {
                thread_id: source.owner_thread_id.clone(),
                server_instance_id: original.threads.capabilities().server_instance_id,
            },
            &source,
            AcpThreadRequest {
                caller: caller.clone(),
                workspace: original
                    .native_workspace(&before, &before.workflow.tasks[0].executions[0])?,
                config: AcpConfig {
                    agent_id: "goal-peer".into(),
                    requested_model_route: None,
                    max_duration: Duration::from_secs(600),
                },
                permission_profile: PermissionProfile::Ask,
                idempotency_key: key.clone(),
            },
        )
        .await?;
    let old_epoch = original.threads.capabilities().server_instance_id;
    original.threads.shutdown().await;
    let stopped = crate::agent_store::DatabaseExecutionStore::new(old_db.clone())
        .read_owner(&old_epoch)
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        stopped.is_some_and(|owner| owner.stopped_at_ms.is_some()),
        "original execution owner was not joined and stopped"
    );
    original.supervisor.shutdown().await;
    old_db.close().await?;
    drop(original);
    let (current, db) = coordinator_with_config(home.path(), &url, &extra).await?;
    let result = async {
        let goal = read(&current, &id).await?;
        let task = &goal.workflow.tasks[0];
        let execution = &task.executions[0];
        let prompt = String::from_utf8(
            crate::goal_artifacts::read(
                &crate::goal_artifacts::root(&current.workspaces, &id),
                &execution
                    .resources
                    .as_ref()
                    .context("saved ACP preparation missing")?
                    .prompt,
            )
            .await?,
        )?;
        Box::pin(current.launch_acp(&goal, task, &execution.id, prompt, "goal-peer", vec![]))
            .await
            .context("recover never-started ACP metadata through shared admission")?;
        let linked = read(&current, &id).await?;
        let binding = linked.workflow.tasks[0].executions[0]
            .thread
            .as_ref()
            .context("original worker metadata was not linked")?;
        assert_eq!(binding.thread_id, worker.thread_id);
        assert_eq!(linked.workflow.tasks[0].executions.len(), 1);
        assert_eq!(linked.workflow.tasks[0].executions[0].id, execution.id);
        let target = ThreadTarget {
            thread_id: binding.thread_id.clone(),
            server_instance_id: current.threads.capabilities().server_instance_id,
        };
        let session = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let turn = current
                    .threads
                    .read_stored_turn(&target, &caller, &binding.turn_id)
                    .await?;
                if let Some(session) = turn.acp {
                    break Ok::<_, anyhow::Error>(session);
                }
                anyhow::ensure!(
                    !turn.status.terminal(),
                    "original ACP admission stopped before selecting a session: {:?}",
                    turn.detail
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert!(!session.session_id.is_empty());
        assert!(
            current
                .threads
                .read_stored_thread_view(&target, &caller)
                .await?
                .recovery
                .is_none()
        );
        assert_eq!(
            current
                .threads
                .find_thread_admission(&target.server_instance_id, &caller, &key)
                .await?
                .context("original Thread key missing")?
                .thread_id,
            worker.thread_id
        );
        let mut captures = tokio::fs::read_dir(home.path().join("captures")).await?;
        let mut initialize = 0;
        let mut new = 0;
        while let Some(file) = captures.next_entry().await? {
            for line in tokio::fs::read_to_string(file.path()).await?.lines() {
                let message: serde_json::Value = serde_json::from_str(line)?;
                initialize += usize::from(message["method"] == "initialize");
                new += usize::from(message["method"] == "session/new");
                assert_ne!(message["method"], "session/load");
                assert_ne!(message["method"], "session/resume");
            }
        }
        assert_eq!(initialize, 1);
        assert_eq!(new, 1, "metadata recovery invented an earlier ACP session");
        Ok(())
    }
    .await;
    current.threads.shutdown().await;
    current.supervisor.shutdown().await;
    db.close().await?;
    if result.is_ok() {
        home.disable_cleanup(false);
    } else if keep {
        eprintln!("Retained ACP metadata fixture: {}", home.path().display());
    }
    result
}

/// Capture the fully committed pre-prompt boundary, then really join the source
/// before transferring only its stopped-owner proof into that exact SQLite image.
/// A fresh process must load the original private nonce; no session/new fallback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_pre_prompt_recovery_loads_only_the_original_stopped_session() -> Result<()> {
    use super::native_recovery::{BarrierStore, Boundary, stopped_source};
    use std::sync::atomic::AtomicBool;
    use tokio::sync::Semaphore;
    for (label, retired, private_present, load_supported, paused) in [
        ("same-session", true, true, true, false),
        ("live-owner", false, true, true, false),
        ("missing-private", true, false, true, false),
        ("unsupported-load", true, true, false, false),
        ("paused-source", true, true, true, true),
    ] {
        let mut home = tempfile::tempdir()?;
        let keep = std::env::var_os("BRO_GOAL_FIXTURE_KEEP_FAILURES").is_some();
        home.disable_cleanup(keep);
        if keep {
            eprintln!(
                "ACP continuation fixture {label}: {}",
                home.path().display()
            );
        }
        let mut extra = mcp_worker_configuration(home.path()).await?;
        extra.push_str("        BRO_GOAL_FIXTURE_PERSIST_SESSION: '1'\n");
        if !load_supported {
            extra.push_str("        BRO_GOAL_FIXTURE_NO_LOAD: '1'\n");
        }
        let url = format!(
            "sqlite://{}?mode=rwc",
            home.path().join("source.db").display()
        );
        let (mut original, db) = coordinator_with_config(home.path(), &url, &extra).await?;
        original.threads.shutdown().await;
        let barrier = Arc::new(BarrierStore {
            inner: Arc::new(crate::agent_store::DatabaseExecutionStore::new(db.clone())),
            boundary: Boundary::AcpSession,
            result_seen: AtomicBool::new(false),
            fired: AtomicBool::new(false),
            ready: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        original.threads = ThreadService::with_store(original.app.clone(), &[], barrier.clone())?;
        let mut destination = None;
        let result: Result<()> = async {
            let id = prepared_worker_attempt(
                &original,
                home.path(),
                Some("goal-peer"),
                Selection::default(),
            )
            .await?;
            let before = read(&original, &id).await?;
            original
                .drive_task(&before, &before.workflow.tasks[0])
                .await?;
            tokio::time::timeout(Duration::from_secs(10), barrier.ready.acquire())
                .await??
                .forget();
            let saved = read(&original, &id).await?;
            let execution = saved.workflow.tasks[0].executions[0].clone();
            let link = execution
                .thread
                .as_ref()
                .context("original ACP binding missing")?
                .clone();
            let old_epoch = original.threads.capabilities().server_instance_id;
            let private = home.path().join("controls/private-session.json");
            let original_private: serde_json::Value =
                serde_json::from_slice(&tokio::fs::read(&private).await?)?;
            assert_eq!(
                original_private["prompts"], 0,
                "source prompt was already dispatched"
            );
            let image = home.path().join("checkpoint.db");
            crate::upgrade_preflight::snapshot(&db, &image).await?;
            if retired {
                stopped_source(&original, &barrier, &execution.workspace).await?;
                original.supervisor.shutdown().await;
            }
            let image_url = format!("sqlite://{}?mode=rwc", image.display());
            let (current, image_db) =
                coordinator_with_config(home.path(), &image_url, &extra).await?;
            destination = Some((current, image_db));
            let (current, image_db) = destination.as_ref().context("destination missing")?;
            if retired {
                let stopped = barrier
                    .read_owner(&old_epoch)
                    .await
                    .map_err(anyhow::Error::msg)?
                    .context("source stopped proof missing")?;
                anyhow::ensure!(
                    stopped.stopped_at_ms.is_some(),
                    "source was not actually stopped"
                );
                let store = crate::agent_store::DatabaseExecutionStore::new(image_db.clone());
                let copied = store
                    .read_owner(&old_epoch)
                    .await
                    .map_err(anyhow::Error::msg)?
                    .context("copied owner missing")?;
                let mut expected = stopped.clone();
                expected.stopped_at_ms = None;
                assert_eq!(copied, expected);
                store
                    .stop_owner(&copied)
                    .await
                    .map_err(anyhow::Error::msg)?;
            }
            if !private_present {
                tokio::fs::remove_file(&private).await?;
            }
            let mut current_goal = read(current, &id).await?;
            current.claim_owner(&current_goal).await?;
            if paused {
                current.goals.execute("local", GoalCommand::Control {
                    operation_id: "pause-acp-continuation".into(), goal_id: id.clone(),
                    expected_revision: current_goal.revision, intent: RunIntent::Pause,
                }, now()?).await?;
                current_goal = read(current, &id).await?;
            }
            let target = ThreadTarget {
                thread_id: link.thread_id.clone(),
                server_instance_id: current.threads.capabilities().server_instance_id,
            };
            current
                .native_workspace(&current_goal, &current_goal.workflow.tasks[0].executions[0])?;
            let restored = current
                .resume_acp_worker(
                    &current_goal,
                    &current_goal.workflow.tasks[0],
                    &current_goal.workflow.tasks[0].executions[0],
                )
                .await;
            if retired && private_present && load_supported && !paused {
                let receipt = restored?;
                assert_eq!(receipt.turn_id, link.turn_id);
                let resumed = tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let turn = current
                            .threads
                            .read_stored_turn(&target, &CallerContext::local(), &link.turn_id)
                            .await?;
                        if turn
                            .acp
                            .as_ref()
                            .is_some_and(|session| session.restored_from.is_some())
                        {
                            break Ok::<_, anyhow::Error>(turn);
                        }
                        anyhow::ensure!(
                            !turn.status.terminal()
                                && turn.status
                                    != bitrouter_orchestrator::turn::TurnStatus::RecoveryRequired,
                            "same session restoration failed: {:?}",
                            turn.detail
                        );
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await??;
                let binding = resumed.acp.context("restored binding missing")?;
                assert_eq!(
                    binding.session_id,
                    original_private["session"]
                        .as_str()
                        .context("private session missing")?
                );
                assert_eq!(
                    tokio::fs::read_to_string(home.path().join("controls/loaded-private-nonce"))
                        .await?,
                    original_private["nonce"]
                        .as_str()
                        .context("nonce missing")?
                );
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let state: serde_json::Value =
                            serde_json::from_slice(&tokio::fs::read(&private).await?)?;
                        if state["prompts"] == 1 {
                            break Ok::<_, anyhow::Error>(());
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await??;
                let permission = tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let turn = current.threads.read_stored_turn(&target, &CallerContext::local(), &link.turn_id).await?;
                        if let Some(permission) = turn.acp_permissions.first() {
                            break Ok::<_, anyhow::Error>(permission.clone());
                        }
                        anyhow::ensure!(!turn.status.terminal(), "restored Turn ended before permission");
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }).await??;
                current.threads.answer_acp_permission(&target, &CallerContext::local(),
                    bitrouter_orchestrator::service::worker::acp::Approval {
                        turn_id: link.turn_id.clone(), request_id: permission.request_id,
                        option_id: "allow-once".into(), idempotency_key: "allow-restored-write".into(),
                    }).await?;
                tokio::fs::write(home.path().join("controls/release"), "release").await?;
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let turn = current.threads.read_stored_turn(&target, &CallerContext::local(), &link.turn_id).await?;
                        if turn.status.terminal() || turn.status == bitrouter_orchestrator::turn::TurnStatus::RecoveryRequired {
                            anyhow::ensure!(turn.status == bitrouter_orchestrator::turn::TurnStatus::Completed
                                && !turn.unknown_effect && turn.acp_stop.as_ref().is_some_and(|stop| stop.confirmed),
                                "restored Turn did not finish safely: {:?}",turn.detail);
                            break Ok::<_, anyhow::Error>(());
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }).await??;
                assert_eq!(tokio::fs::read_to_string(execution.workspace.join("left.txt")).await?, "LEFT\n");
                let mut methods = Vec::new();
                let mut files = tokio::fs::read_dir(home.path().join("captures")).await?;
                while let Some(file) = files.next_entry().await? {
                    for line in tokio::fs::read_to_string(file.path()).await?.lines() {
                        let event: serde_json::Value = serde_json::from_str(line)?;
                        methods.push(event["method"].as_str().unwrap_or("").to_owned());
                    }
                }
                assert_eq!(
                    methods
                        .iter()
                        .filter(|method| method.as_str() == "session/new")
                        .count(),
                    1
                );
                assert_eq!(
                    methods
                        .iter()
                        .filter(|method| method.as_str() == "session/load")
                        .count(),
                    1
                );
                assert_eq!(
                    methods
                        .iter()
                        .filter(|method| method.as_str() == "session/prompt")
                        .count(),
                    1
                );
                assert_eq!(read(current, &id).await?.workflow.tasks[0].executions.len(), 1);
                eprintln!("ACP_CONTINUATION_PROOF {}", serde_json::json!({
                    "scenario":label,"thread_id":link.thread_id,"turn_id":link.turn_id,
                    "session_id":binding.session_id,"restored_from":binding.restored_from,
                    "new_run_id":binding.run_id,"session_new":1,"session_load":1,"prompt":1,
                    "turn_completed":true,"process_stop_confirmed":true,"checked_file":"LEFT\n",
                    "scope":"Controlled SQLite pre-prompt snapshot, actual source join, Rust peer persisted private nonce; no vendor/provider qualification"
                }));
            } else if !retired || paused {
                assert!(restored.is_err(), "live or paused source resumed");
            } else {
                restored?; // shared recovery admission precedes protocol settlement
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let turn = current
                            .threads
                            .read_stored_turn(&target, &CallerContext::local(), &link.turn_id)
                            .await?;
                        if turn.status == bitrouter_orchestrator::turn::TurnStatus::RecoveryRequired
                            || turn.status.terminal()
                        {
                            anyhow::ensure!(
                                turn.acp
                                    .as_ref()
                                    .is_none_or(|session| session.restored_from.is_none()),
                                "unconfirmed session was adopted"
                            );
                            break Ok::<_, anyhow::Error>(());
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await??;
                if private_present {
                    let state: serde_json::Value =
                        serde_json::from_slice(&tokio::fs::read(&private).await?)?;
                    assert_eq!(state["prompts"], 0, "failed load submitted a prompt");
                }
            }
            Ok(())
        }
        .await;
        // Release a still-live source even when assertions fail; all owned tasks
        // and actual protocol processes are joined before reporting the result.
        barrier.release.add_permits(1);
        original.threads.shutdown().await;
        original.supervisor.shutdown().await;
        if let Some((current, image_db)) = destination {
            current.threads.shutdown().await;
            current.supervisor.shutdown().await;
            image_db.close().await?;
        }
        db.close().await?;
        if result.is_ok() {
            home.disable_cleanup(false);
        }
        result.with_context(|| format!("ACP continuation scenario {label}"))?;
    }
    Ok(())
}
