//! Persistent Core consumes the same checked binding for roots, children and wakeups.
use super::*;
mod observations;
use crate::storage::checkpoint::snapshot::collaboration::{Action, Work};

fn work(text: &str) -> Work {
    Work {
        text: text.into(),
        model: None,
        effort: None,
        acceptance_criteria: vec![],
        required_materials: vec![],
        task_scope: None,
        fresh_context: false,
        independent_review: false,
    }
}

#[tokio::test]
async fn native_child_followup_reconnect_and_queued_root_inherit_the_fixed_owned_binding()
-> Result<()> {
    let mut provider = Provider::start(false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, directory.path()).await?;
    let checked = tokio::time::timeout(Duration::from_secs(25), async {
        let (headers, key) = ready(&fixture, &route).await?;
        let r1 = headers
            .get("bitrouter-execution-revision")
            .context("R1 binding")?
            .to_str()?
            .try_into()?;
        let mut executions = std::collections::BTreeSet::new();
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let session = NativeSession::open_authenticated(
            app,
            headers,
            "family",
            &workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await?;
        let core = session.core().clone();
        let mut task = input();
        task.max_concurrent_subagents = Some(1);
        let first = core
            .start("first-root", core.head().await.state_revision, task.clone())
            .await?;
        let root = first.assigned_ids.get("agent_id").context("root")?.clone();
        let spawned = core
            .collaborate(
                "child",
                core.head().await.state_revision,
                &root,
                Action::Spawn {
                    task: work("child work"),
                },
            )
            .await?;
        let child = spawned
            .assigned_ids
            .get("agent_id")
            .context("child")?
            .clone();
        let first_state = core.drive().await?;
        observations::phase(
            &fixture,
            &mut provider,
            &first_state,
            (&root, &child),
            &r1,
            &mut executions,
        )
        .await
        .context("first root/child/completion-wakeup evidence")?;
        let row = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT payload FROM bro_managed_sessions",
            ))
            .await?
            .context("native journal")?;
        let saved: Value = serde_json::from_str(&row.try_get::<String>("", "payload")?)?;
        let grant = serde_json::from_value(saved["journal"]["grant"].clone())?;
        core.disconnect().await;
        core.reconnect(&grant, &core.head().await).await?;

        let r2 = draft(&fixture, &route, "family-r2", 7).await?;
        result(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Activate(
                        fixture.tenant.clone(),
                        bitrouter_ai::tenancy::configuration::Activation {
                            operation_id: "family-activate".try_into()?,
                            expected_active_revision: None,
                            revision_id: r2.revision_id,
                            expected_authority_revision: fixture
                                .service
                                .current()
                                .map_err(failure)?
                                .revision,
                            installation_id: fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        core.start(
            "second-root",
            core.head().await.state_revision,
            task.clone(),
        )
        .await?;
        core.collaborate(
            "wake-child",
            core.head().await.state_revision,
            &root,
            Action::Followup {
                agent_id: child.clone(),
                task: work("followup work"),
            },
        )
        .await?;
        let second_state = core.drive().await?;
        ensure!(
            first_state.run.as_ref().context("first run")?.run_id
                != second_state.run.as_ref().context("second run")?.run_id
        );
        observations::phase(
            &fixture,
            &mut provider,
            &second_state,
            (&root, &child),
            &r1,
            &mut executions,
        )
        .await
        .context("retained R1 followup/completion-wakeup evidence")?;
        let sent_before_revoke = provider.calls();
        core.start("third-root", core.head().await.state_revision, task.clone())
            .await?;
        core.collaborate(
            "pending-child",
            core.head().await.state_revision,
            &root,
            Action::Followup {
                agent_id: child.clone(),
                task: work("must not dispatch"),
            },
        )
        .await?;
        core.enqueue(
            "pending-root",
            core.head().await.state_revision,
            task.clone(),
        )
        .await?;
        change_authority(&fixture, &key, &route, "revoke").await?;
        ensure!(
            core.collaborate(
                "forbidden-wakeup",
                core.head().await.state_revision,
                &root,
                Action::Message {
                    agent_id: child,
                    text: "no new work".into()
                }
            )
            .await
            .is_err()
        );
        ensure!(
            core.start("first-root", first.state_revision.saturating_sub(1), task)
                .await
                .is_err(),
            "operation replay bypassed current authority"
        );
        ensure!(
            core.resume_queue("forbidden-resume", core.head().await.state_revision)
                .await
                .is_err()
        );
        ensure!(core.reconnect(&grant, &core.head().await).await.is_err());
        // Core may persist a terminal rejection; it must never send the queued
        // root or followup. The actual native owner still performs ACK/release.
        if let Err(error) = core.drive().await {
            ensure!(matches!(
                error.code,
                bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope
                    | bitrouter_ai::core::protocol::ErrorCode::CheckpointUnavailable
            ));
        }
        session.close().await?;
        ensure!(
            provider.calls() == sent_before_revoke,
            "revoked root/followup reached provider"
        );
        ensure!(
            matches!(
                provider.received.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ),
            "unmatched or revoked provider request"
        );
        observations::receipts(&fixture, &executions).await?;
        ensure!(fixture.service.host_slots_for_test() == 1 && fixture.service.current().is_ok());
        Ok(())
    })
    .await
    .context("native family deadline")
    .and_then(|outcome| outcome);
    finish(&fixture, provider, checked).await
}

#[tokio::test]
async fn cancelled_persistent_native_waiter_retains_host_and_provider_custody_through_drain()
-> Result<()> {
    let mut provider = Provider::start(true).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, directory.path()).await?;
    let checked = async {
        let (headers, key) = ready(&fixture, &route).await?;
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let mut session = NativeSession::open_authenticated(
            app,
            headers,
            "cancelled",
            &workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await?;
        let run = AbortOnDropHandle::new(tokio::spawn(async move {
            session
                .run(input(), Arc::new(Approval), CancellationToken::new())
                .await
        }));
        provider.receive().await?;
        change_authority(&fixture, &key, &route, "revoke").await?;
        run.abort();
        ensure!(
            matches!(tokio::time::timeout(TIMEOUT, run).await?, Err(error) if error.is_cancelled())
        );
        ensure!(
            fixture.service.host_slots_for_test() == 0,
            "waiter cancellation released the native owner"
        );
        let limits = Limits {
            active_executions: 1,
            queued_work: 0,
            learning_jobs: 0,
        };
        ensure!(matches!(
            fixture
                .service
                .admission
                .acquire(&fixture.tenant, &limits)
                .now_or_never(),
            Some(Err(ErrorCode::CapacityExceeded))
        ));
        let mut shutdown = Box::pin(fixture.service.shutdown());
        ensure!(
            futures::poll!(&mut shutdown).is_pending(),
            "native drain falsely acknowledged provider cleanup"
        );
        ensure!(fixture.service.host_slots_for_test() == 0);
        provider.state.release.cancel();
        tokio::time::timeout(TIMEOUT, shutdown).await??;
        tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
        ensure!(fixture.service.host_slots_for_test() == 1 && provider.calls() == 1);
        let row = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT payload FROM bro_managed_sessions",
            ))
            .await?
            .context("released native journal")?;
        let saved: Value = serde_json::from_str(&row.try_get::<String>("", "payload")?)?;
        ensure!(saved["journal"]["released"] == true);
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await
}
