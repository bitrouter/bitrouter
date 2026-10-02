use super::*;
use bitrouter_orchestrator::core::accounting::work::{CostWorkKind, CostWorkState};

async fn start_with_material(session: &CoreSession) -> Result<String, CoreError> {
    session
        .signals(
            "inventory",
            signal_update(session, vec![material("v1", "document", false)]).await,
        )
        .await?;
    let receipt = session
        .start("input", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    Ok(receipt.assigned_ids["run_id"].clone())
}

#[tokio::test]
async fn material_outcome_and_cost_wait_for_ack_and_replay_once() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("material.resolved")));
    let (session, executor, _) =
        setup(vec![output(vec![text("done")])], harness.clone(), false).await?;
    let run_id = start_with_material(&session).await?;
    let request_id = harness.material_requests.lock().await[0].0.clone();
    let before = session.snapshot().await;
    let resolving = tokio::spawn({
        let session = session.clone();
        let request_id = request_id.clone();
        async move {
            session
                .material_result(
                    "resolved",
                    &request_id,
                    Some(material("v1", "document", true)),
                    None,
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    assert_eq!(session.snapshot().await.cost_work, before.cost_work);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    harness.resume.add_permits(1);
    let receipt = resolving.await??;
    let accepted = session.snapshot().await;
    let fetch = &accepted.cost_work[&run_id].work[&request_id];
    assert_eq!(fetch.state, CostWorkState::OutcomeRecorded);
    assert_eq!(fetch.unknown_cost_reason, "harness_cost_not_reported");
    assert!(fetch.elapsed_ms.is_none());
    assert!(fetch.token_estimate.is_none());
    assert_eq!(
        session
            .material_result(
                "resolved",
                &request_id,
                Some(material("v1", "document", true)),
                None,
            )
            .await?,
        receipt
    );
    assert_eq!(session.snapshot().await.cost_work, accepted.cost_work);
    assert_eq!(
        session
            .material_result(
                "second-result",
                &request_id,
                Some(material("v1", "document", true)),
                None,
            )
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::OperationConflict)
    );
    let persisted: SessionSnapshot = {
        let store = harness.store.lock().await;
        serde_json::from_value(
            store
                .batches
                .last()
                .ok_or("checkpoint")?
                .decode(&store.limits)?
                .checkpoint
                .state,
        )?
    };
    assert_eq!(persisted.cost_work, accepted.cost_work);
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Completed)
    );
    Ok(())
}

struct LostMaterialAck(Arc<Harness>);

#[async_trait]
impl HarnessPort for LostMaterialAck {
    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        let lose_ack = batch
            .decode(&Limits::default())?
            .events
            .iter()
            .any(|event| event.kind == "material.resolved");
        let ack = self.0.commit(batch).await?;
        if lose_ack {
            return Err(CoreError::rejected(
                ErrorCode::CheckpointUnavailable,
                "lost material ACK",
            ));
        }
        Ok(ack)
    }

    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        self.0.send(message).await
    }
}

#[tokio::test]
async fn lost_material_ack_retains_intent_until_durable_reconciliation() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        Vec::new(),
        Arc::new(LostMaterialAck(harness.clone())),
        false,
    )
    .await?;
    let run_id = start_with_material(&session).await?;
    let request_id = harness.material_requests.lock().await[0].0.clone();
    let error = session
        .material_result(
            "resolved",
            &request_id,
            Some(material("v1", "document", true)),
            None,
        )
        .await
        .err()
        .ok_or("missing lost ACK error")?;
    assert_eq!(error.commit_status, CommitStatus::Unknown);
    let live = session.snapshot().await;
    assert_eq!(
        live.cost_work[&run_id].work[&request_id].state,
        CostWorkState::IntentRecorded
    );
    assert!(!live.signals.requests[&request_id].resolved);
    let persisted: SessionSnapshot = {
        let store = harness.store.lock().await;
        serde_json::from_value(
            store
                .batches
                .last()
                .ok_or("checkpoint")?
                .decode(&store.limits)?
                .checkpoint
                .state,
        )?
    };
    assert_eq!(
        persisted.cost_work[&run_id].work[&request_id].state,
        CostWorkState::OutcomeRecorded
    );
    assert!(persisted.signals.requests[&request_id].resolved);
    assert!(session.drive().await.is_err());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn late_material_result_keeps_cancelled_run_owner_when_reused() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![text("done")])], harness.clone(), false).await?;
    let old_run = start_with_material(&session).await?;
    let request_id = harness.material_requests.lock().await[0].0.clone();
    let origin = session.snapshot().await.signals.requests[&request_id]
        .origin
        .clone()
        .ok_or("origin")?;
    session
        .cancel_run("cancel", session.head().await.state_revision, &old_run)
        .await?;
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Cancelled)
    );
    let new_run = session
        .start("next", session.head().await.state_revision, input())
        .await?
        .assigned_ids["run_id"]
        .clone();
    let waiting = session.drive().await?;
    assert_eq!(harness.material_requests.lock().await.len(), 1);
    assert!(waiting.cost_work[&new_run].work.is_empty());
    assert_ne!(
        waiting.root_turn().ok_or("turn")?.agent_turn_id,
        origin.agent_turn_id
    );
    session
        .material_result(
            "resolved",
            &request_id,
            Some(material("v1", "document", true)),
            None,
        )
        .await?;
    let resolved = session.snapshot().await;
    let fetch = &resolved.cost_work[&old_run].work[&request_id];
    assert_eq!(fetch.state, CostWorkState::OutcomeRecorded);
    assert_eq!(fetch.agent_turn_id, origin.agent_turn_id);
    assert_eq!(fetch.agent_id, origin.agent_id);
    assert_eq!(
        resolved.signals.requests[&request_id].origin.as_ref(),
        Some(&origin)
    );
    assert!(resolved.cost_work[&new_run].work.is_empty());
    {
        let store = harness.store.lock().await;
        let payload = store
            .batches
            .last()
            .ok_or("checkpoint")?
            .decode(&store.limits)?;
        assert_eq!(payload.events[0].kind, "material.resolved");
        assert_eq!(payload.events[0].run_id.as_deref(), Some(old_run.as_str()));
        assert_eq!(
            payload.events[0].agent_id.as_deref(),
            Some(origin.agent_id.as_str())
        );
    }
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(done.cost_work[&old_run], resolved.cost_work[&old_run]);
    assert!(
        done.cost_work[&new_run]
            .work
            .values()
            .all(|work| work.kind != CostWorkKind::MaterialFetch)
    );
    Ok(())
}

#[tokio::test]
async fn children_share_one_material_fetch_owned_by_its_first_consumer() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(
        (0..10).map(|_| output(vec![text("done")])).collect(),
        harness.clone(),
        false,
    )
    .await?;
    let mut reference = material("v1", "document", false);
    reference.required = false;
    session
        .signals(
            "inventory",
            signal_update(&session, vec![reference.clone()]).await,
        )
        .await?;
    let accepted = session
        .start("input", session.head().await.state_revision, input())
        .await?;
    let root = &accepted.assigned_ids["agent_id"];
    let run_id = &accepted.assigned_ids["run_id"];
    let mut children = Vec::new();
    for operation in ["first-child", "second-child"] {
        let mut task = work(operation);
        task.required_materials.push(reference.material_id.clone());
        children.push(
            session
                .collaborate(
                    operation,
                    session.head().await.state_revision,
                    root,
                    Action::Spawn { task },
                )
                .await?
                .assigned_ids["agent_id"]
                .clone(),
        );
    }
    let waiting = session.drive().await?;
    assert_eq!(harness.material_requests.lock().await.len(), 1);
    let request_id = harness.material_requests.lock().await[0].0.clone();
    let fetch = &waiting.cost_work[run_id].work[&request_id];
    assert!(children.contains(&fetch.agent_id));
    assert_eq!(
        waiting.cost_work[run_id]
            .work
            .values()
            .filter(|work| work.kind == CostWorkKind::MaterialFetch)
            .count(),
        1
    );
    reference.content = Some("document".into());
    session
        .material_result("resolved", &request_id, Some(reference), None)
        .await?;
    {
        let store = harness.store.lock().await;
        let payload = store
            .batches
            .last()
            .ok_or("checkpoint")?
            .decode(&store.limits)?;
        assert_eq!(
            payload.events[0].agent_id.as_deref(),
            Some(fetch.agent_id.as_str())
        );
    }
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    for child in children {
        let turn = done.agents[&child].turn.as_ref().ok_or("child turn")?;
        assert_eq!(turn.steps[0].materials[0].version, "v1");
    }
    assert_eq!(
        done.cost_work[run_id]
            .work
            .values()
            .filter(|work| work.kind == CostWorkKind::MaterialFetch)
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn unavailable_and_stale_material_fetches_retain_unknown_expenditure() -> TestResult {
    for unavailable in [true, false] {
        let harness = Arc::new(Harness::new(None, None));
        let (session, _, _) =
            setup(vec![output(vec![text("done")])], harness.clone(), false).await?;
        let run_id = start_with_material(&session).await?;
        let request_id = harness.material_requests.lock().await[0].0.clone();
        if unavailable {
            session
                .material_result("unavailable", &request_id, None, Some("not found".into()))
                .await?;
        }
        session
            .signals(
                "new-inventory",
                signal_update(&session, vec![material("v2", "new document", false)]).await,
            )
            .await?;
        if !unavailable {
            assert_eq!(
                session
                    .material_result(
                        "stale",
                        &request_id,
                        Some(material("v1", "document", true)),
                        None
                    )
                    .await
                    .err()
                    .map(|error| error.code),
                Some(ErrorCode::StaleRevision)
            );
        }
        let waiting = session.drive().await?;
        let old = &waiting.cost_work[&run_id].work[&request_id];
        assert_eq!(
            old.state,
            if unavailable {
                CostWorkState::OutcomeRecorded
            } else {
                CostWorkState::IntentRecorded
            }
        );
        assert_eq!(old.unknown_cost_reason, "harness_cost_not_reported");
        assert!(old.elapsed_ms.is_none());
        assert!(old.token_estimate.is_none());
        let new_request = harness.material_requests.lock().await[1].0.clone();
        assert_eq!(
            waiting.cost_work[&run_id].work[&new_request].state,
            CostWorkState::IntentRecorded
        );
        session
            .material_result(
                "new-result",
                &new_request,
                Some(material("v2", "new document", true)),
                None,
            )
            .await?;
        let done = session.drive().await?;
        assert_eq!(done.cost_work[&run_id].work[&request_id], *old);
        assert_eq!(
            done.cost_work[&run_id].work[&new_request].state,
            CostWorkState::OutcomeRecorded
        );
        assert_eq!(
            done.cost_work[&run_id]
                .work
                .values()
                .filter(|work| work.kind == CostWorkKind::MaterialFetch)
                .count(),
            2
        );
    }
    Ok(())
}
