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
