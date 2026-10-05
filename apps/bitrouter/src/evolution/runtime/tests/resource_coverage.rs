use super::*;
use crate::acp_trajectory::Recorder;
use crate::acp_trajectory::checkpoint::types::{Checkpoint, ResourceObservation};
use crate::evolution::inventory::{COVERAGE_KIND, CoverageRegistration};

async fn prompt(recorder: &Recorder, id: &str, call: u64, kind: CaptureKind) -> Result<()> {
    let payload = if kind == CaptureKind::Request {
        json!({"sessionId":id,"prompt":[{"type":"text","text":"Continue the fixture."}]})
    } else {
        json!({"result":{"stopReason":"end_turn"}})
    };
    recorder
        .record(CaptureEvent {
            direction: CaptureDirection::Client,
            kind,
            call_id: Some(call),
            method: "session/prompt".into(),
            payload,
        })
        .await?;
    Ok(())
}

async fn freeze(
    fixture: &Fixture,
    identity: &SessionIdentity,
) -> Result<(Checkpoint, ResourceObservation)> {
    let canonical = CanonicalStore::new(fixture.assembled.db.clone());
    let head = canonical.transcript(identity).await?.session.head;
    let cp = canonical.freeze_checkpoint(identity, head).await?;
    let resource = canonical
        .observe_checkpoint_resources(identity, &cp.checkpoint_id)
        .await?;
    Ok((cp, resource))
}

#[tokio::test]
async fn priced_gateway_requests_complete_a_stopped_prefix_and_exclude_later_work() -> Result<()> {
    let fixture = fixture(false).await?;
    let (identity, recorder) = recording(&fixture, "priced", "fixture", true).await?;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    pipeline
        .execute(request(&identity, "first", "coding")?)
        .await?;
    let (_, active) = freeze(&fixture, &identity).await?;
    assert!(
        !active.metering_complete,
        "a running prompt cannot establish final cost"
    );
    prompt(&recorder, "priced", 2, CaptureKind::Response).await?;
    let (cp, complete) = freeze(&fixture, &identity).await?;
    assert!(
        complete.metering_complete,
        "{:?}",
        complete.gateway_coverage
    );
    assert_eq!(complete.known_cost_micro_usd, 28);
    assert_eq!(complete.requests.len(), 1);
    let canonical = CanonicalStore::new(fixture.assembled.db.clone());
    assert_eq!(
        canonical
            .observe_checkpoint_resources(&identity, &cp.checkpoint_id)
            .await?
            .observation_id,
        complete.observation_id
    );
    prompt(&recorder, "priced", 3, CaptureKind::Request).await?;
    pipeline
        .execute(request(&identity, "second", "coding")?)
        .await?;
    prompt(&recorder, "priced", 3, CaptureKind::Response).await?;
    let previous = canonical
        .observe_checkpoint_resources(&identity, &cp.checkpoint_id)
        .await?;
    assert!(previous.metering_complete);
    assert_eq!(previous.known_cost_micro_usd, 28);
    assert_eq!(previous.requests.len(), 1);
    let (_, cumulative) = freeze(&fixture, &identity).await?;
    assert!(cumulative.metering_complete);
    assert_eq!(cumulative.known_cost_micro_usd, 56);
    assert_eq!(cumulative.requests.len(), 2);
    recorder
        .record(CaptureEvent {
            direction: CaptureDirection::Client,
            kind: CaptureKind::Disconnected,
            call_id: None,
            method: "disconnected".into(),
            payload: json!({"clean":true}),
        })
        .await?;
    assert!(
        pipeline
            .execute(request(&identity, "first", "coding")?)
            .await
            .is_err(),
        "closing capture cannot allow an already-accounted request ID to execute again"
    );
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("HTTP observations missing")?
            .len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn post_stop_calls_extend_own_resources_without_changing_content() -> Result<()> {
    let fixture = fixture(false).await?;
    let (identity, recorder) = recording(&fixture, "auxiliary", "fixture", true).await?;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    pipeline
        .execute(request(&identity, "main", "coding")?)
        .await?;
    prompt(&recorder, "auxiliary", 2, CaptureKind::Response).await?;
    let (checkpoint, original) = freeze(&fixture, &identity).await?;
    assert!(original.metering_complete);
    assert_eq!(original.known_cost_micro_usd, 28);

    // Maintained workers can request a title or other auxiliary inference after
    // prompt completion without first appending another ACP content event.
    pipeline
        .execute(request(&identity, "after-stop", "coding")?)
        .await?;
    let canonical = CanonicalStore::new(fixture.assembled.db.clone());
    let extended = canonical
        .observe_checkpoint_resources(&identity, &checkpoint.checkpoint_id)
        .await?;
    assert_eq!(
        extended.known_cost_micro_usd, 56,
        "the current session's post-stop call must not disappear at an equal content watermark"
    );
    assert!(extended.metering_complete);
    assert_eq!(extended.requests.len(), 2);
    assert!(extended.revision > original.revision);
    assert_eq!(
        canonical
            .checkpoint_content(&identity, &checkpoint.checkpoint_id)
            .await?
            .checkpoint
            .prefix_digest,
        checkpoint.prefix_digest
    );
    assert_eq!(
        canonical.transcript(&identity).await?.session.head,
        checkpoint.watermark
    );

    let inventory = fixture.assembled.evolution.inventory();
    let BeginResult::Tracked(pending) = inventory
        .begin(
            "local",
            "controller",
            "pending-tail",
            Some(InventoryBinding {
                identity: identity.clone(),
                connection_id: recorder.connection_id().into(),
                watermark: checkpoint.watermark,
            }),
        )
        .await?
    else {
        anyhow::bail!("tail request not tracked");
    };
    let unfinished = canonical
        .observe_checkpoint_resources(&identity, &checkpoint.checkpoint_id)
        .await?;
    assert_eq!(unfinished.requests.len(), 3);
    assert!(!unfinished.metering_complete);
    assert_eq!(unfinished.known_cost_micro_usd, 56);
    inventory
        .settle(
            &pending,
            ExecutionSettlement {
                settled_at: chrono::Utc::now().to_rfc3339(),
                hops: vec![],
                final_model: "fixture".into(),
                final_provider: "fixture".into(),
                error_code: None,
                duration_ms: 1,
                outcome: None,
                final_metered_cost_micro_usd: Some(7),
                total_cost_micro_usd: Some(7),
            },
        )
        .await?;
    inventory
        .terminal(&pending, ExecutionOutcome::Completed)
        .await?;
    let complete = canonical
        .observe_checkpoint_resources(&identity, &checkpoint.checkpoint_id)
        .await?;
    assert!(complete.metering_complete);
    assert_eq!(complete.known_cost_micro_usd, 63);

    prompt(&recorder, "auxiliary", 3, CaptureKind::Request).await?;
    pipeline
        .execute(request(&identity, "next-turn", "coding")?)
        .await?;
    prompt(&recorder, "auxiliary", 3, CaptureKind::Response).await?;
    let prior = canonical
        .observe_checkpoint_resources(&identity, &checkpoint.checkpoint_id)
        .await?;
    assert_eq!(
        prior.requests.len(),
        3,
        "the following prompt is outside the earlier resource boundary"
    );
    assert_eq!(prior.known_cost_micro_usd, 63);
    let (_, cumulative) = freeze(&fixture, &identity).await?;
    assert_eq!(cumulative.known_cost_micro_usd, 91);
    assert_eq!(cumulative.requests.len(), 4);
    Ok(())
}

#[tokio::test]
async fn fork_resources_union_inherited_requests_and_exclude_later_parent_work() -> Result<()> {
    let fixture = fixture(false).await?;
    let (parent, recorder) = recording(&fixture, "parent", "fixture", true).await?;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    pipeline
        .execute(request(&parent, "parent-before-fork", "coding")?)
        .await?;
    prompt(&recorder, "parent", 2, CaptureKind::Response).await?;
    for (kind, payload) in [
        (CaptureKind::Request, json!({"sessionId":"parent"})),
        (
            CaptureKind::Response,
            json!({"result":{"sessionId":"child"}}),
        ),
    ] {
        recorder
            .record(CaptureEvent {
                direction: CaptureDirection::Client,
                kind,
                call_id: Some(3),
                method: "session/fork".into(),
                payload,
            })
            .await?;
    }
    let child = SessionIdentity {
        native_session_id: "child".into(),
        ..parent.clone()
    };
    let (_, boundary_at_fork_request) = freeze(&fixture, &parent).await?;
    assert!(
        !boundary_at_fork_request.metering_complete,
        "a child response after the parent's current prefix must not close it retroactively"
    );
    pipeline
        .execute(request(&parent, "parent-tail-after-fork", "coding")?)
        .await?;
    prompt(&recorder, "parent", 4, CaptureKind::Request).await?;
    pipeline
        .execute(request(&parent, "parent-after-fork", "coding")?)
        .await?;
    prompt(&recorder, "parent", 4, CaptureKind::Response).await?;
    prompt(&recorder, "child", 5, CaptureKind::Request).await?;
    pipeline
        .execute(request(&child, "child-request", "coding")?)
        .await?;
    prompt(&recorder, "child", 5, CaptureKind::Response).await?;
    let (_, child_resources) = freeze(&fixture, &child).await?;
    assert!(
        child_resources.metering_complete,
        "{:?}",
        child_resources.gateway_coverage
    );
    assert_eq!(child_resources.known_cost_micro_usd, 56);
    assert_eq!(
        child_resources
            .requests
            .iter()
            .map(|r| r.request_id.as_str())
            .collect::<Vec<_>>(),
        vec!["child-request", "parent-before-fork"]
    );
    let (parent_cp, parent_resources) = freeze(&fixture, &parent).await?;
    assert!(
        parent_resources.metering_complete,
        "{:?}",
        parent_resources.gateway_coverage
    );
    assert_eq!(parent_resources.requests.len(), 3);
    let canonical = CanonicalStore::new(fixture.assembled.db.clone());
    canonical.delete(&child).await?;
    assert!(
        canonical
            .checkpoint_content(&parent, &parent_cp.checkpoint_id)
            .await
            .is_err(),
        "deleting referenced fork completion evidence invalidates the parent checkpoint"
    );
    Ok(())
}

#[tokio::test]
async fn unresolved_requests_are_retained_and_known_other_sessions_are_excluded() -> Result<()> {
    let fixture = fixture(false).await?;
    let (identity, recorder) = recording(&fixture, "ours", "fixture", true).await?;
    let other = session(&fixture, "other", "fixture").await?;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    pipeline
        .execute(request(&other, "other-request", "coding")?)
        .await?;
    pipeline
        .execute(request(&identity, "our-request", "coding")?)
        .await?;
    prompt(&recorder, "ours", 2, CaptureKind::Response).await?;
    let (cp, scoped) = freeze(&fixture, &identity).await?;
    assert!(scoped.metering_complete, "{:?}", scoped.gateway_coverage);
    assert_eq!(scoped.requests.len(), 1);
    assert_eq!(scoped.requests[0].request_id, "our-request");
    prompt(&recorder, "ours", 3, CaptureKind::Request).await?;
    let unknown = SessionIdentity {
        native_session_id: "unrecorded".into(),
        ..identity.clone()
    };
    pipeline
        .execute(request(&unknown, "unresolved", "coding")?)
        .await?;
    prompt(&recorder, "ours", 3, CaptureKind::Response).await?;
    let (_, incomplete) = freeze(&fixture, &identity).await?;
    assert!(!incomplete.metering_complete);
    assert_eq!(incomplete.unassigned_request_ids, vec!["unresolved"]);
    assert_eq!(incomplete.known_cost_micro_usd, 28);
    let prior = CanonicalStore::new(fixture.assembled.db.clone())
        .observe_checkpoint_resources(&identity, &cp.checkpoint_id)
        .await?;
    assert!(
        prior.metering_complete,
        "later unresolved traffic does not enter an older prefix"
    );
    let inventory = fixture.assembled.evolution.inventory();
    let (_, saved): (_, GatewayRequest) = inventory
        .scoped_store("local", "controller")?
        .get(crate::evolution::inventory::INVENTORY_KIND, "unresolved")
        .await?
        .context("unresolved request disappeared")?;
    assert!(saved.binding.is_none());
    assert_eq!(
        saved
            .settlement
            .context("unresolved settlement missing")?
            .outcome,
        Some(ExecutionOutcome::Completed)
    );
    assert!(
        pipeline
            .execute(request(&unknown, "unresolved", "coding")?)
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("HTTP observations missing")?
            .len(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn unresolved_post_stop_calls_stay_unknown_only_through_their_native_interval() -> Result<()>
{
    let fixture = fixture(false).await?;
    let (identity, recorder) = recording(&fixture, "unknown-tail", "fixture", true).await?;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    pipeline
        .execute(request(&identity, "known", "coding")?)
        .await?;
    prompt(&recorder, "unknown-tail", 2, CaptureKind::Response).await?;
    let (checkpoint, _) = freeze(&fixture, &identity).await?;
    // A different native session advances the shared connection while our
    // content head stays stopped. Its events cannot make our unknown tail free.
    for (kind, payload) in [
        (CaptureKind::Request, json!({"cwd":"/fixture"})),
        (
            CaptureKind::Response,
            json!({"result":{"sessionId":"side-session"}}),
        ),
    ] {
        recorder
            .record(CaptureEvent {
                direction: CaptureDirection::Client,
                kind,
                call_id: Some(4),
                method: "session/new".into(),
                payload,
            })
            .await?;
    }
    let unknown = SessionIdentity {
        native_session_id: "not-recorded".into(),
        ..identity.clone()
    };
    pipeline
        .execute(request(&unknown, "first-tail", "coding")?)
        .await?;
    let canonical = CanonicalStore::new(fixture.assembled.db.clone());
    let unresolved = canonical
        .observe_checkpoint_resources(&identity, &checkpoint.checkpoint_id)
        .await?;
    assert!(!unresolved.metering_complete);
    assert_eq!(unresolved.unassigned_request_ids, vec!["first-tail"]);
    prompt(&recorder, "unknown-tail", 3, CaptureKind::Request).await?;
    pipeline
        .execute(request(&unknown, "second-turn-unknown", "coding")?)
        .await?;
    prompt(&recorder, "unknown-tail", 3, CaptureKind::Response).await?;
    let prior = canonical
        .observe_checkpoint_resources(&identity, &checkpoint.checkpoint_id)
        .await?;
    assert!(!prior.metering_complete);
    assert_eq!(
        prior.unassigned_request_ids,
        vec!["first-tail"],
        "the original tail remains unknown, but a later prompt's unknown request is outside its interval"
    );
    let (_, latest) = freeze(&fixture, &identity).await?;
    assert_eq!(
        latest.unassigned_request_ids,
        vec!["first-tail", "second-turn-unknown"]
    );
    Ok(())
}

#[tokio::test]
async fn missing_handshake_disables_new_trials_and_cannot_be_repaired_retroactively() -> Result<()>
{
    let fixture = fixture(false).await?;
    let runtime = &fixture.assembled.evolution;
    runtime
        .register("local", definition("coding", "candidate"))
        .await?;
    runtime
        .service("local")?
        .set_mode(EvolutionMode::Manual, None)
        .await?;
    let (identity, recorder) = recording(&fixture, "legacy", "fixture", false).await?;
    fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?
        .execute(request(&identity, "legacy-request", "coding")?)
        .await?;
    let enrollment = runtime
        .service("local")?
        .enrollment(&identity)
        .await?
        .context("enrollment missing")?;
    assert!(enrollment.assignments.is_empty());
    assert_eq!(
        runtime.executions(&identity).await?[0]
            .route_guard_reason
            .as_deref(),
        Some("gateway_inventory_unavailable")
    );
    assert!(
        runtime
            .inventory()
            .register_capture(recorder.connection_id(), "local", "controller")
            .await
            .is_err()
    );
    prompt(&recorder, "legacy", 2, CaptureKind::Response).await?;
    let (_, resource) = freeze(&fixture, &identity).await?;
    assert!(!resource.metering_complete);
    assert_eq!(
        resource.known_cost_micro_usd, 28,
        "known subtotal remains inspectable"
    );
    Ok(())
}

#[tokio::test]
async fn late_settlement_revises_resources_without_mutating_checkpoint_content() -> Result<()> {
    let fixture = fixture(false).await?;
    let (identity, recorder) = recording(&fixture, "pending", "fixture", true).await?;
    let inventory = fixture.assembled.evolution.inventory();
    let head = CanonicalStore::new(fixture.assembled.db.clone())
        .transcript(&identity)
        .await?
        .session
        .head;
    let BeginResult::Tracked(observed) = inventory
        .begin(
            "local",
            "controller",
            "late-receipt",
            Some(InventoryBinding {
                identity: identity.clone(),
                connection_id: recorder.connection_id().into(),
                watermark: head,
            }),
        )
        .await?
    else {
        anyhow::bail!("request not tracked");
    };
    prompt(&recorder, "pending", 2, CaptureKind::Response).await?;
    let (cp, pending) = freeze(&fixture, &identity).await?;
    assert!(!pending.metering_complete);
    assert_eq!(pending.unpriced_requests, 1);
    // Explicit controlled receipt: validates asynchronous accounting mechanics,
    // not measured provider spending or historical trajectory quality.
    inventory
        .settle(
            &observed,
            ExecutionSettlement {
                settled_at: chrono::Utc::now().to_rfc3339(),
                hops: vec![],
                final_model: "fixture".into(),
                final_provider: "fixture".into(),
                error_code: None,
                duration_ms: 1,
                outcome: None,
                final_metered_cost_micro_usd: Some(37),
                total_cost_micro_usd: Some(37),
            },
        )
        .await?;
    let canonical = CanonicalStore::new(fixture.assembled.db.clone());
    assert!(
        !canonical
            .observe_checkpoint_resources(&identity, &cp.checkpoint_id)
            .await?
            .metering_complete
    );
    inventory
        .terminal(&observed, ExecutionOutcome::Completed)
        .await?;
    let complete = canonical
        .observe_checkpoint_resources(&identity, &cp.checkpoint_id)
        .await?;
    assert!(
        complete.metering_complete,
        "{:?}",
        complete.gateway_coverage
    );
    assert_eq!(complete.known_cost_micro_usd, 37);
    assert!(complete.revision > pending.revision);
    assert_eq!(
        canonical
            .checkpoint_content(&identity, &cp.checkpoint_id)
            .await?
            .checkpoint
            .prefix_digest,
        cp.prefix_digest
    );
    Ok(())
}

#[tokio::test]
async fn capture_acknowledgement_fences_namespace_preexisting_requests_and_runtime_restart()
-> Result<()> {
    let fixture = fixture(false).await?;
    let recorder = CanonicalStore::new(fixture.assembled.db.clone())
        .recorder(RecordingScope {
            owner: "local".into(),
            source: "fixture".into(),
            controller_instance_id: Some("startup".into()),
            route_scope_id: Some("local".into()),
        })
        .await?;
    let inventory = fixture.assembled.evolution.inventory();
    assert!(
        inventory
            .register_capture(recorder.connection_id(), "other", "startup")
            .await
            .is_err()
    );
    let first = inventory
        .register_capture(recorder.connection_id(), "local", "startup")
        .await?;
    assert_eq!(
        inventory
            .register_capture(recorder.connection_id(), "local", "startup")
            .await?
            .runtime_epoch,
        first.runtime_epoch
    );
    assert!(matches!(
        inventory
            .begin("local", "startup", "before-capture", None)
            .await?,
        BeginResult::Tracked(_)
    ));
    assert!(
        inventory
            .register_capture(recorder.connection_id(), "local", "startup")
            .await
            .is_err()
    );
    let restarted = GatewayInventory::new(fixture.assembled.db.clone());
    let BeginResult::Tracked(after) = restarted
        .begin("local", "startup", "after-restart", None)
        .await?
    else {
        anyhow::bail!("request not tracked");
    };
    assert!(!after.coverage_ready);
    let (_, coverage): (_, CoverageRegistration) =
        EvolutionStore::new(fixture.assembled.db.clone(), "local")?
            .get(COVERAGE_KIND, recorder.connection_id())
            .await?
            .context("coverage missing")?;
    assert_eq!(
        coverage.invalid_reason.as_deref(),
        Some("gateway_runtime_changed_during_capture")
    );
    Ok(())
}
