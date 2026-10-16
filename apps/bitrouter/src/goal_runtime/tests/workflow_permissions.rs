//! Actual permission RPC history across teardown and local reconciliation.
use super::*;
use bitrouter_orchestrator::goal::decision::PermissionDecision;

fn pending_right(goal: &GoalSnapshot) -> Option<&PermissionDecision> {
    let execution = goal
        .workflow
        .tasks
        .iter()
        .find(|task| task.spec.id == "right")?
        .executions
        .last()?;
    goal.workflow.decisions.iter().find(|decision| {
        decision.execution_id == execution.id
            && decision.state == bitrouter_orchestrator::goal::decision::PermissionState::Pending
    })
}

async fn pause_with_unanswered_permission(
    fixture: &Fixture,
    id: &str,
) -> Result<(
    PermissionDecision,
    GoalSnapshot,
    crate::supervisor::ReplayBatch,
)> {
    // Teardown targets RIGHT's unanswered permission after LEFT's actual write.
    let unanswered = wait_for(&fixture.socket, id, |goal| {
        fixture.controls.join("completed-left").exists() && pending_right(goal).is_some()
    })
    .await?;
    let decision = pending_right(&unanswered)
        .context("original unanswered right decision missing")?
        .clone();
    control(&fixture.socket, id, "teardown-pause", RunIntent::Pause).await?;
    let stopped = wait_for(&fixture.socket, id, |goal| {
        goal.workflow
            .tasks
            .iter()
            .filter(|task| !task.executions.is_empty())
            .all(|task| {
                task.executions.last().is_some_and(|execution| {
                    matches!(execution.stop, Some(StopState::Confirmed { .. }))
                })
            })
    })
    .await?;
    wait_for(&fixture.socket, id, |goal| {
        goal.workflow.decisions.iter().any(|current| {
            current.id == decision.id
                && matches!(
                    current.state,
                    bitrouter_orchestrator::goal::decision::PermissionState::Obsolete { .. }
                )
        })
    })
    .await?;
    let SessionResponse::Authorized { grant } = fixture
        .supervisor
        .dispatch(SessionRequest::authorize(
            "teardown-evidence".into(),
            BTreeSet::from([crate::supervisor::SessionScope::Transcript]),
        ))
        .await?
    else {
        anyhow::bail!("teardown transcript grant missing")
    };
    let SessionResponse::Events { replay } = fixture
        .supervisor
        .dispatch(SessionRequest::authorized(
            &grant,
            SessionCommand::Events {
                run_id: decision
                    .run_id
                    .clone()
                    .context("ACP permission run is missing")?,
                after_seq: 0,
            },
        ))
        .await?
    else {
        anyhow::bail!("teardown transcript missing")
    };
    let resolution = replay.events.iter().find(|event| matches!(&event.kind,
                    crate::supervisor::SessionEventKind::PermissionResolved { permission_id, .. } if permission_id == &decision.permission_id))
                    .context("actual rejection missing")?;
    let value = serde_json::to_value(&resolution.kind)?;
    assert_eq!(value["option_id"], "deny-once");
    assert_eq!(value["origin"], "teardown");

    Ok((decision, stopped, replay))
}

fn assert_paused_without_delivery(stopped: &GoalSnapshot) {
    assert_eq!(stopped.run_intent, RunIntent::Pause);
    assert!(!stopped.workflow.completion.completed_for(1));
    assert_eq!(
        stopped
            .workflow
            .tasks
            .iter()
            .map(|task| task.executions.len())
            .sum::<usize>(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teardown_denial_has_controller_origin_and_does_not_become_user_consent() -> Result<()> {
    let mut setup = Setup::new().await?;
    setup.agent_env("BRO_GOAL_FIXTURE_RIGHT_MANUAL");
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        fixture.running(&id).await?;
        let (_, stopped, _) = pause_with_unanswered_permission(&fixture, &id).await?;
        assert_paused_without_delivery(&stopped);
        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmed_stop_keeps_original_permission_history_after_cold_reopen() -> Result<()> {
    let mut setup = Setup::new().await?;
    setup.agent_env("BRO_GOAL_FIXTURE_RIGHT_MANUAL");
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        fixture.running(&id).await?;
        let (decision, stopped, replay) = pause_with_unanswered_permission(&fixture, &id).await?;
        let cold = Supervisor::open(fixture.source.clone(), fixture.routing.clone()).await?;
        let SessionResponse::Authorized { grant } = cold
            .dispatch(SessionRequest::authorize(
                "cold-teardown-reader".into(),
                BTreeSet::from([
                    crate::supervisor::SessionScope::Transcript,
                    crate::supervisor::SessionScope::Peek,
                ]),
            ))
            .await?
        else {
            anyhow::bail!("cold transcript grant missing")
        };
        let SessionResponse::Events { replay: restored } = cold
            .dispatch(SessionRequest::authorized(
                &grant,
                SessionCommand::Events {
                    run_id: decision
                        .run_id
                        .clone()
                        .context("ACP permission run is missing")?,
                    after_seq: 0,
                },
            ))
            .await?
        else {
            anyhow::bail!("cold stopped transcript missing")
        };
        assert!(restored.history_complete);
        assert_eq!(
            serde_json::to_value(&restored)?,
            serde_json::to_value(&replay)?
        );
        let SessionResponse::Snapshot { snapshot } = cold
            .dispatch(SessionRequest::authorized(
                &grant,
                SessionCommand::Snapshot {
                    run_id: decision
                        .run_id
                        .clone()
                        .context("ACP permission run is missing")?,
                },
            ))
            .await?
        else {
            anyhow::bail!("cold stopped snapshot missing")
        };
        assert_eq!(snapshot.process, ProcessState::Stopped);
        assert_eq!(snapshot.last_seq, replay.snapshot_seq);
        assert_paused_without_delivery(&stopped);
        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_local_failed_tool_can_reconcile_without_a_user_permission_submission()
-> Result<()> {
    let mut setup = Setup::new().await?;
    setup.agent_env("BRO_GOAL_FIXTURE_RIGHT_DENIED_LOCAL");
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        fixture.running(&id).await?; let (decision, _, _) = pause_with_unanswered_permission(&fixture, &id).await?;
                    let original = decision.execution_id.clone();
                    assert!(!fixture.controls.join("completed-right").exists());
                    assert!(matches!(decision.state, bitrouter_orchestrator::goal::decision::PermissionState::Pending));
                    control(&fixture.socket, &id, "explicit-resume-denied-local", RunIntent::Run).await?;
                    let reconciled = wait_for(&fixture.socket, &id, |goal| goal.workflow.tasks.iter().flat_map(|task| &task.executions)
                        .find(|execution| execution.id == original).is_some_and(|execution| matches!(execution.reconciliation,
                            Some(bitrouter_orchestrator::goal::workflow::LocalReconciliation::Verified { .. })))).await?;
                    let execution = reconciled.workflow.tasks.iter().flat_map(|task| &task.executions)
                        .find(|execution| execution.id == original).context("original denied execution missing")?;
                    let Some(bitrouter_orchestrator::goal::workflow::LocalReconciliation::Verified { transcript, .. }) = &execution.reconciliation
                        else { anyhow::bail!("local failed terminal not reconciled") };
                    let root = fixture.root(&id);
                    let proof: serde_json::Value = serde_json::from_slice(&crate::goal_artifacts::read(&root, transcript).await?)?;
                    assert_eq!(proof["permission_submissions"].as_array().map(Vec::len), Some(0));
                    assert_eq!(proof["controller_denials"].as_array().map(Vec::len), Some(1));
                    assert!(!reconciled.workflow.completion.completed_for(1));

        Ok(())
    }.await;
    fixture.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sparse_permission_keeps_original_rpc_and_uses_prior_tool_context_for_delivery()
-> Result<()> {
    let mut setup = Setup::new().await?;
    setup.agent_env("BRO_GOAL_FIXTURE_SPARSE_PERMISSION");
    let fixture = setup.start().await?;
    let result = async {
        let id = fixture.create().await?;

        fixture.running(&id).await?;
        fixture.release().await?;
        let completed = fixture.completed(&id, 1).await?;
        let root = fixture.root(&id);
        use bitrouter_orchestrator::goal::decision::Event;
        let mut requests = BTreeMap::new();
        let mut delivered = BTreeSet::new();
        let mut after = 0;
        loop {
            let GoalReply::History { page } = rpc(
                &fixture.socket,
                GoalCommand::History {
                    goal_id: id.clone(),
                    after,
                    cutoff: Some(completed.revision),
                    limit: 16,
                },
            )
            .await?
            else {
                anyhow::bail!("original permission history missing")
            };
            assert_eq!(page.cutoff, completed.revision);
            for operation in page.operations {
                if let bitrouter_orchestrator::goal::OperationKind::Workflow { change } =
                    operation.kind
                    && let Change::Decision { event } = *change
                {
                    match event {
                        Event::Observe { decision } => {
                            requests.insert(decision.id.clone(), decision);
                        }
                        Event::Result {
                            decision_id,
                            submitted: true,
                            ..
                        } => {
                            delivered.insert(decision_id);
                        }
                        _ => {}
                    }
                }
            }
            let Some(next) = page.next_after else { break };
            assert!(next > after);
            after = next;
        }
        assert_eq!(requests.len(), 3);
        assert_eq!(delivered, requests.keys().cloned().collect());
        let SessionResponse::Authorized { grant } = fixture
            .supervisor
            .dispatch(SessionRequest::authorize(
                "permission-context-evidence".into(),
                BTreeSet::from([crate::supervisor::SessionScope::Transcript]),
            ))
            .await?
        else {
            anyhow::bail!("permission transcript grant missing")
        };
        for decision in requests.values() {
            let bytes = crate::goal_artifacts::read(&root, &decision.request).await?;
            let request: crate::supervisor::PendingPermissionSnapshot =
                serde_json::from_slice(&bytes)?;
            let tool = serde_json::to_value(&request.tool_call)?;
            assert_eq!(tool["locations"], serde_json::json!([]));
            assert_eq!(tool["rawInput"], serde_json::json!({}));
            let material = crate::goal_permission_material::decision_value(&bytes, decision)?;
            assert!(
                material["earlier_tool_context"]["declarations"]
                    .as_array()
                    .is_some_and(|events| events.len() == 2)
            );
            let SessionResponse::Events { replay } = fixture
                .supervisor
                .dispatch(SessionRequest::authorized(
                    &grant,
                    SessionCommand::Events {
                        run_id: decision
                            .run_id
                            .clone()
                            .context("ACP permission run is missing")?,
                        after_seq: 0,
                    },
                ))
                .await?
            else {
                anyhow::bail!("original controller transcript missing")
            };
            let original = replay
                .events
                .iter()
                .find_map(|event| match &event.kind {
                    crate::supervisor::SessionEventKind::Permission { permission }
                        if permission.permission_id == decision.permission_id =>
                    {
                        Some(permission)
                    }
                    _ => None,
                })
                .context("original controller request missing")?;
            assert_eq!(
                crate::goal_permission_material::original_bytes(&bytes)?,
                serde_json::to_vec(original)?
            );
        }
        Ok(())
    }
    .await;
    fixture.finish(result).await
}
