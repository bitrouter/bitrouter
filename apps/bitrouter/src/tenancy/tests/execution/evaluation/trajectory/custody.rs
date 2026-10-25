use super::*;
use crate::{
    gateway::{
        error::BitrouterError,
        observability::event::EventBus,
        pipeline::settlement::{SettlementContext, SettlementRecorder},
    },
    tenancy::{
        actor::Job,
        eval::observation::{RequiredObservation, TrajectoryStart},
        execution::TenantExecution,
        trajectory::{Begin, Recorder},
        transaction::Disposition,
    },
    trajectory::{
        correlation::CorrelationEvidence,
        store::{CorrelateAndBegin, GuardedRouteInput},
        types::KeyedDigest,
    },
};
use tokio::{sync::oneshot, time::Instant};

fn input() -> Result<(CorrelateAndBegin, CorrelationEvidence)> {
    let digest = KeyedDigest::parse(format!("hmac-sha256:test-key:{}", "1".repeat(64)))?;
    let evidence = CorrelationEvidence {
        native_parent_id: None,
        full_input_digest: digest.as_str().into(),
        ancestor_prefix_digests: Vec::new(),
        ancestor_prefixes_truncated: false,
        starts_with_prior_turns: false,
        canonical_input_bytes: 12,
    };
    Ok((
        CorrelateAndBegin {
            request_id: "owned-request".into(),
            new_episode_id: "owned-episode".into(),
            event_id: "owned-start".into(),
            correlation_key_id: "test-key".into(),
            native_parent_id: None,
            native_parent_digest: None,
            full_input_digest: digest,
            ancestor_prefix_digests: Vec::new(),
            ancestor_prefixes_truncated: false,
            starts_with_prior_turns: false,
            canonical_input_bytes: 12,
            protocol: "chat_completions".into(),
            captured_at: "2026-10-25T00:00:00Z".into(),
            guarded_route: Some(GuardedRouteInput {
                route_event_id: "owned-route".into(),
                guard_event_id: "owned-guard".into(),
                policy_name: "selected".into(),
                route_projection: "semantic_route/v1|code:generation|implement|normal".into(),
                request_key: "agent_trace/v2|edit|normal".into(),
                baseline_tier: Some("economy".into()),
                baseline_effort: None,
                tier_efforts: Default::default(),
                preset: Some("selected".into()),
                projection: crate::gateway::routing::workflow::ir::RouteProjection::parse_key(
                    "agent_trace/v2|edit|normal",
                )
                .context("projection")?,
                candidate_tier: Some("economy".into()),
                policy_digest: crate::policy_lock::semantic_digest(&document())?,
                experiment: None,
                route_measurement: None,
                policy: guard(),
                carries_tools: false,
                tool_use_tier: None,
                tool_safe_tiers: Default::default(),
            }),
        },
        evidence,
    ))
}
fn context(execution: &TenantExecution, failed: bool) -> SettlementContext {
    SettlementContext {
        operation: bitrouter_ai::types::ModelOperation::Generation,
        request_id: execution.id().to_string(),
        caller: execution.caller(),
        target: None,
        model_id: "model-a".into(),
        reasoning_effort: None,
        provider_id: "mock".into(),
        account_label: None,
        prompt_tokens: 0,
        completion_tokens: 0,
        reasoning_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        usage_origin: bitrouter_ai::types::UsageOrigin::Unknown,
        raw_usage: None,
        web_search_count: 0,
        media_input_count: 0,
        media_output_count: 0,
        server_tool_calls: Vec::new(),
        streamed: false,
        request_duration_ms: 5,
        upstream_duration_ms: None,
        ttft_ms: None,
        generation_duration_ms: None,
        first_token_kind: None,
        finish_reason: None,
        error: failed.then(|| BitrouterError::bad_request("fixture stopped")),
        events: EventBus::default(),
    }
}
async fn enqueue(
    fixture: &Fixture,
    execution: &Arc<TenantExecution>,
    observation: &Arc<RequiredObservation>,
    expired: bool,
) -> Result<
    oneshot::Receiver<
        std::result::Result<crate::tenancy::trajectory::BeginReply, crate::tenancy::error::Failure>,
    >,
> {
    observation.start_requested()?;
    let (input, evidence) = input()?;
    let (reply, receive) = oneshot::channel();
    // This direct actor injection deliberately drops the response waiter below.
    // The real HTTP tests independently exercise production adapter construction.
    fixture
        .service
        .commands
        .send(Job::TrajectoryBegin(Box::new(Begin {
            execution: execution.clone(),
            observation: observation.clone(),
            input: Arc::new(input),
            evidence,
            observe: true,
            reservation: crate::trajectory::store::owned::headroom::Reservation::new(
                16 * 1024,
                true,
            )?,
            deadline: if expired {
                Instant::now() - Duration::from_secs(1)
            } else {
                Instant::now() + crate::tenancy::eval::WORK
            },
            reply,
        })))
        .await
        .map_err(|_| anyhow::anyhow!("actor closed"))?;
    Ok(receive)
}

#[tokio::test]
async fn committed_begin_survives_lost_reply_and_requires_atomic_terminal() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let (fixture, revision) =
            fixture("https://example.invalid/v1", LearningMode::Observe).await?;
        let headers = credential(&fixture, &revision).await?;
        let execution = admitted(&fixture, &headers).await?;
        let pipeline = execution.pipeline()?;
        let observation = execution
            .trajectory_observation()
            .context("observation")?
            .clone();
        let registration = fixture.service.pipelines.register_observed(
            execution.id(),
            pipeline,
            observation.clone(),
        )?;
        observation.entered()?;
        drop(enqueue(&fixture, &execution, &observation, false).await?);
        while !observation.start_resolved() {
            tokio::task::yield_now().await;
        }
        ensure!(matches!(
            observation.trajectory_start()?,
            TrajectoryStart::Committed(_)
        ));
        ensure!(fixture.service.cleanup.disposition() == Disposition::Committed);
        // Keeping the external ticket also keeps shutdown pending before the
        // recorder has discharged the committed-start obligation.
        let mut drain = Box::pin(fixture.service.pipelines.close_and_drain());
        ensure!(futures::poll!(&mut drain).is_pending());
        drop(drain);
        Recorder::new(&execution, observation.clone(), Default::default())
            .record(&mut context(&execution, true))
            .await?;
        ensure!(observation.confirmed());
        duplicate_publication_and_startup_frontier(&fixture, &execution, &observation).await?;
        ensure!(count(&fixture.db, "trajectory_requests", "status = 'failed'").await? == 1);
        for table in [
            "trajectory_outbox",
            "eval_subjects",
            "eval_results",
            "eval_admission_events",
        ] {
            ensure!(count(&fixture.db, table, "1=1").await? == 1);
        }
        ensure!(count(&fixture.db, "trajectory_outbox", "delivered_at IS NULL").await? == 0);
        drop((registration, execution, observation));
        fixture.service.pipelines.close_and_drain().await?;
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn only_confirmed_no_start_can_skip_after_begin_was_requested() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let (fixture, revision) =
            fixture("https://example.invalid/v1", LearningMode::Observe).await?;
        let headers = credential(&fixture, &revision).await?;
        let execution = admitted(&fixture, &headers).await?;
        let pipeline = execution.pipeline()?;
        let observation = execution
            .trajectory_observation()
            .context("observation")?
            .clone();
        let registration = fixture.service.pipelines.register_observed(
            execution.id(),
            pipeline,
            observation.clone(),
        )?;
        observation.entered()?;
        let rejected = enqueue(&fixture, &execution, &observation, true)
            .await?
            .await?;
        let rejected = rejected.err().context("expired begin succeeded")?;
        ensure!(rejected.0.code == bitrouter_ai::tenancy::operation::ErrorCode::CapacityExceeded);
        ensure!(fixture.service.cleanup.disposition() == Disposition::NoTransaction);
        ensure!(matches!(
            observation.trajectory_start()?,
            TrajectoryStart::NoStart
        ));
        Recorder::new(&execution, observation.clone(), Default::default())
            .record(&mut context(&execution, true))
            .await?;
        drop((registration, execution, observation));
        fixture.service.pipelines.close_and_drain().await?;
        ensure!(fixture.service.current().is_ok());
        ensure!(count(&fixture.db, "trajectory_requests", "1=1").await? == 0);
        ensure!(count(&fixture.db, "eval_subjects", "1=1").await? == 0);
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn committed_begin_cannot_be_erased_by_absent_recorder_or_no_start_claim() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let (fixture, revision) =
            fixture("https://example.invalid/v1", LearningMode::Observe).await?;
        let headers = credential(&fixture, &revision).await?;
        let execution = admitted(&fixture, &headers).await?;
        let pipeline = execution.pipeline()?;
        let observation = execution
            .trajectory_observation()
            .context("observation")?
            .clone();
        let registration = fixture.service.pipelines.register_observed(
            execution.id(),
            pipeline,
            observation.clone(),
        )?;
        observation.entered()?;
        enqueue(&fixture, &execution, &observation, false)
            .await?
            .await?
            .map_err(failure)?;
        observation.no_start(Disposition::NoTransaction);
        ensure!(matches!(
            observation.trajectory_start()?,
            TrajectoryStart::Committed(_)
        ));
        // No terminal callback: closing a producer is not proof that no write ran.
        drop((registration, execution, observation));
        ensure!(fixture.service.pipelines.close_and_drain().await.is_err());
        ensure!(fixture.service.current().is_err());
        ensure!(count(&fixture.db, "trajectory_requests", "status = 'started'").await? == 1);
        ensure!(fixture.service.shutdown().await.is_err());
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn begin_deadline_rollback_proves_no_start_only_after_cleanup_ack() -> Result<()> {
    use crate::tenancy::transaction::Phase;
    let (fixture, revision) = fixture("https://example.invalid/v1", LearningMode::Observe).await?;
    let headers = credential(&fixture, &revision).await?;
    let execution = admitted(&fixture, &headers).await?;
    let pipeline = execution.pipeline()?;
    let observation = execution
        .trajectory_observation()
        .context("observation")?
        .clone();
    let registration = fixture.service.pipelines.register_observed(
        execution.id(),
        pipeline,
        observation.clone(),
    )?;
    observation.entered()?;
    let gate = fixture.service.cleanup.hold(Phase::Active);
    let receive = enqueue(&fixture, &execution, &observation, false).await?;
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    ensure!(!observation.start_resolved());
    tokio::time::pause();
    tokio::time::advance(crate::tenancy::eval::WORK + Duration::from_millis(1)).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.service.cleanup.read_stopped.notified(),
    )
    .await?;
    ensure!(!observation.start_resolved());
    fixture.service.cleanup.release_gate();
    tokio::time::resume();
    let error = tokio::time::timeout(TIMEOUT, receive)
        .await??
        .err()
        .context("expired begin succeeded")?;
    ensure!(error.0.code == bitrouter_ai::tenancy::operation::ErrorCode::CapacityExceeded);
    ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
    ensure!(matches!(
        observation.trajectory_start()?,
        TrajectoryStart::NoStart
    ));
    Recorder::new(&execution, observation.clone(), Default::default())
        .record(&mut context(&execution, true))
        .await?;
    drop((registration, execution, observation));
    fixture.service.pipelines.close_and_drain().await?;
    ensure!(count(&fixture.db, "trajectory_requests", "1=1").await? == 0);
    ensure!(fixture.service.current().is_ok());
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn overdue_begin_commit_stays_owned_and_cannot_become_no_start() -> Result<()> {
    use crate::tenancy::transaction::Phase;
    let (fixture, revision) = fixture("https://example.invalid/v1", LearningMode::Observe).await?;
    let headers = credential(&fixture, &revision).await?;
    let execution = admitted(&fixture, &headers).await?;
    let pipeline = execution.pipeline()?;
    let observation = execution
        .trajectory_observation()
        .context("observation")?
        .clone();
    let registration = fixture.service.pipelines.register_observed(
        execution.id(),
        pipeline,
        observation.clone(),
    )?;
    observation.entered()?;
    let gate = fixture.service.cleanup.hold(Phase::Committing);
    drop(enqueue(&fixture, &execution, &observation, false).await?);
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    tokio::time::pause();
    tokio::time::advance(crate::tenancy::eval::WORK + Duration::from_millis(1)).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.service.cleanup.wait_fenced(),
    )
    .await?;
    tokio::time::resume();
    ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
    ensure!(!observation.start_resolved());
    let mut drain = Box::pin(fixture.service.pipelines.close_and_drain());
    ensure!(futures::poll!(&mut drain).is_pending());
    drop(drain);
    fixture.service.cleanup.release_gate();
    tokio::time::timeout(TIMEOUT, async {
        while !observation.start_resolved() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    ensure!(fixture.service.cleanup.disposition() == Disposition::Committed);
    ensure!(count(&fixture.db, "trajectory_requests", "status = 'started'").await? == 1);
    drop((registration, execution, observation));
    ensure!(fixture.service.pipelines.close_and_drain().await.is_err());
    ensure!(fixture.service.shutdown().await.is_err());
    Ok(())
}

async fn duplicate_publication_and_startup_frontier(
    fixture: &Fixture,
    execution: &TenantExecution,
    observation: &RequiredObservation,
) -> Result<()> {
    use crate::{
        core::eval::store::OwnedError,
        storage::operation_budget::OperationBudget,
        tenancy::transaction::{Control, Proof},
        trajectory::types::{OutboxWrite, RequestStatus, Settlement},
    };
    use sea_orm::TransactionTrait;
    let TrajectoryStart::Committed(proof) = observation.trajectory_start()? else {
        anyhow::bail!("missing committed proof")
    };
    let event=fixture.db.query_one(Statement::from_string(fixture.db.get_database_backend(),"SELECT e.event_json FROM trajectory_events e JOIN trajectory_requests r ON e.event_id=r.settlement_event_id".to_owned())).await?.context("terminal event")?;
    let outbox = fixture
        .db
        .query_one(Statement::from_string(
            fixture.db.get_database_backend(),
            "SELECT outbox_id,topic,payload_json,created_at FROM trajectory_outbox".to_owned(),
        ))
        .await?
        .context("terminal outbox")?;
    // Stored PK is physical. The versioned identity projection carries the
    // logical outbox ID, and the typed API validates that projection before use.
    let request = fixture
        .db
        .query_one(Statement::from_string(
            fixture.db.get_database_backend(),
            "SELECT logical_identity_json FROM trajectory_requests".to_owned(),
        ))
        .await?
        .context("request identity")?;
    let identity: serde_json::Value =
        serde_json::from_str(&request.try_get::<String>("", "logical_identity_json")?)?;
    let logical = identity
        .get("ids")
        .and_then(|ids| ids.get("settlement_outbox_id"))
        .and_then(|v| v.as_str())
        .context("logical terminal outbox")?;
    let settlement = Settlement {
        event: serde_json::from_str(&event.try_get::<String>("", "event_json")?)?,
        status: RequestStatus::Failed,
        outbox: Some(OutboxWrite {
            outbox_id: logical.into(),
            topic: outbox.try_get("", "topic")?,
            payload: serde_json::from_str(&outbox.try_get::<String>("", "payload_json")?)?,
            created_at: outbox.try_get("", "created_at")?,
        }),
    };
    let check = || Ok(());
    let budget = OperationBudget::new(&check);
    let tx = fixture.db.begin().await?;
    let eval = execution.eval_store();
    ensure!(
        !eval
            .publish_trajectory_in(&tx, &proof, &settlement, &budget)
            .await?
    );
    let envelope = settlement
        .outbox
        .as_ref()
        .and_then(|o| o.payload.evaluation.as_deref())
        .context("envelope")?;
    ensure!(matches!(
        eval.submit_owned_in(&tx, &envelope.result, &check).await,
        Err(OwnedError::Rejected(
            bitrouter_ai::tenancy::operation::ErrorCode::Forbidden
        ))
    ));
    tx.rollback().await?;
    ensure!(count(&fixture.db, "eval_admission_events", "1=1").await? == 1);
    let control = Control::new(Proof::new(Arc::new(std::sync::atomic::AtomicBool::new(
        true,
    ))));
    let tx = control.begin(&fixture.db).await?;
    crate::tenancy::trajectory_profile::validate_startup(&tx, &fixture.installation, &control)
        .await?;
    control.rollback(tx).await?;
    fixture
        .db
        .execute_unprepared("UPDATE trajectory_outbox SET delivered_at=NULL")
        .await?;
    let control = Control::new(Proof::new(Arc::new(std::sync::atomic::AtomicBool::new(
        true,
    ))));
    let tx = control.begin(&fixture.db).await?;
    ensure!(
        crate::tenancy::trajectory_profile::validate_startup(&tx, &fixture.installation, &control)
            .await
            .is_err()
    );
    control.rollback(tx).await?;
    let owner = crate::trajectory::store::TrajectoryStore::for_execution(execution).owner()?;
    ensure!(
        crate::trajectory::store::TrajectoryStore::new(fixture.db.clone())
            .pending_outbox(&owner)
            .await?
            .is_empty()
    );
    fixture
        .db
        .execute_unprepared("UPDATE trajectory_outbox SET delivered_at=created_at")
        .await?;
    Ok(())
}
