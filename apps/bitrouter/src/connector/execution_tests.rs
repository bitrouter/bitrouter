use std::{error::Error, time::Duration};

use super::{
    config::ConnectorConfig,
    controller::{Admission, Controller, ControllerError, OperatorAttestation, RequestIdentity},
    journal::{ExecutionRecord, ExecutionState, TerminalEvidence},
    observer::{
        Freshness, MemberMetrics, MemberSnapshot, ObservationStatus, PoolSnapshot, PoolSummary,
        SourceFreshness,
    },
    policy::PauseReason,
};

type TestResult = Result<(), Box<dyn Error>>;

fn configuration(directory: &std::path::Path) -> Result<ConnectorConfig, Box<dyn Error>> {
    let yaml = format!(
        "version: 1\npool:\n  id: test\n  model: test/model\n  scope:\n    kind: static_members\n  members:\n    - id: a\n      inference_endpoint: http://127.0.0.1:9999/v1\n      metrics_endpoint: http://127.0.0.1:9999/metrics\n      engines: ['0']\nobservation:\n  timeout_ms: 10\n  max_body_bytes: 1024\n  max_sample_age_ms: 2000\nexecution:\n  enabled: true\n  exclusive_pool_owner: true\n  state_dir: {}\n  auth_bearer_env: CONNECTOR_TEST_TOKEN\n  max_dispatch_rps: 1000\n  burst: 1\n  max_extra_inflight: 2\n  max_records: 20\n  max_input_tokens: 128\n  max_output_tokens: 16\n  max_deadline_ms: 2000\n  max_request_bytes: 1024\n  max_response_bytes: 1024\n  stream_idle_timeout_ms: 1000\n  lease_ms: 1000\n  update_interval_ms: 10\n  health_window_ms: 10\n  max_running_requests: 10\n  max_waiting_requests: 0\n  max_kv_cache_usage_fraction: 0.8\n",
        directory.display()
    );
    Ok(ConnectorConfig::from_yaml(&yaml)?)
}

fn snapshot() -> PoolSnapshot {
    let now = chrono::Utc::now().timestamp_millis();
    let metrics = MemberMetrics {
        running_requests: 0,
        waiting_requests: 0,
        kv_cache_usage_fraction_max: 0.1,
    };
    PoolSnapshot {
        pool_id: "test".into(),
        model: "test/model".into(),
        observed_at_ms: now,
        scope_evidence: super::observer::scope_evidence(),
        capabilities: super::capabilities::capabilities(),
        members: vec![MemberSnapshot {
            member_id: "a".into(),
            declared_engines: vec!["0".into()],
            metrics_scope_validated: true,
            status: ObservationStatus::Observed,
            reason: None,
            process_started_at_seconds: Some(1.0),
            freshness: Freshness {
                collection_completed_at_ms: Some(now),
                collection_age_ms: Some(0),
                oldest_source_sample_at_ms: Some(now),
                oldest_source_sample_age_ms: Some(0),
                source: SourceFreshness::ExplicitTimestamps,
            },
            metrics: Some(metrics.clone()),
        }],
        summary: PoolSummary {
            declared_members: 1,
            observed_members: 1,
            metrics: Some(metrics),
        },
    }
}

async fn healthy(controller: &Controller) -> TestResult {
    assert_eq!(
        controller.update_snapshot(snapshot()).await?.reason,
        Some(PauseReason::HealthWindow)
    );
    tokio::time::sleep(Duration::from_millis(15)).await;
    assert_eq!(controller.update_snapshot(snapshot()).await?.reason, None);
    Ok(())
}

fn identity(controller: &Controller, id: &str) -> RequestIdentity {
    RequestIdentity {
        request_id: id.into(),
        digest: "a".repeat(64),
        pool_id: "test".into(),
        pool_generation: controller.generation().into(),
        model: "test/model".into(),
        member_id: "a".into(),
        input_tokens: 10,
        max_output_tokens: 10,
        deadline_ms: chrono::Utc::now().timestamp_millis() + 1500,
    }
}

#[test]
fn enabled_execution_rejects_equivalent_inference_paths() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut config = configuration(directory.path())?;
    let mut member = config.pool.members[0].clone();
    member.id = "b".into();
    member.inference_endpoint.push('/');
    member.metrics_endpoint = "http://127.0.0.1:9998/metrics".into();
    config.pool.members.push(member);
    assert!(config.validate().is_err());
    Ok(())
}

async fn reserve(controller: &Controller, id: &str) -> Result<ExecutionRecord, Box<dyn Error>> {
    match controller.reserve(identity(controller, id)).await? {
        Admission::Reserved(record) => Ok(record),
        Admission::Existing(_) => Err("expected a new reservation".into()),
    }
}

async fn await_owner_release(directory: &std::path::Path) -> TestResult {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match crate::tenancy::ownership::acquire_path(
                &directory.join("connector.sqlite3"),
                false,
            ) {
                Ok(lease) => {
                    drop(lease);
                    return Ok::<(), Box<dyn Error>>(());
                }
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn journal_requires_initialization_excludes_second_owner_and_survives_alias() -> TestResult {
    let directory = tempfile::tempdir()?;
    let config = configuration(directory.path())?;
    assert!(matches!(
        Controller::open(config.clone(), false).await,
        Err(ControllerError::StateNotInitialized)
    ));
    let first = Controller::open(config.clone(), true).await?;
    assert!(matches!(
        Controller::open(config.clone(), true).await,
        Err(ControllerError::OwnershipUnavailable)
    ));
    #[cfg(unix)]
    {
        let alias_dir = tempfile::tempdir()?;
        let alias = alias_dir.path().join("authority");
        std::os::unix::fs::symlink(directory.path(), &alias)?;
        assert!(matches!(
            Controller::open(configuration(&alias)?, false).await,
            Err(ControllerError::OwnershipUnavailable)
        ));
    }
    drop(first);
    await_owner_release(directory.path()).await?;
    let second = Controller::open(config, false).await?;
    assert_eq!(second.status().await?.owner_epoch, 2);
    Ok(())
}

#[tokio::test]
async fn rate_burst_inflight_and_dedupe_share_one_atomic_authority() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut config = configuration(directory.path())?;
    config
        .execution
        .as_mut()
        .ok_or("execution absent")?
        .max_dispatch_rps = 1.0;
    let controller = Controller::open(config, true).await?;
    healthy(&controller).await?;
    tokio::time::sleep(Duration::from_millis(1005)).await;
    healthy(&controller).await?;
    let (a, b) = tokio::join!(
        controller.reserve(identity(&controller, "a")),
        controller.reserve(identity(&controller, "b"))
    );
    let a = match a? {
        Admission::Reserved(record) => record,
        _ => return Err("reservation expected".into()),
    };
    let b = match b? {
        Admission::Reserved(record) => record,
        _ => return Err("reservation expected".into()),
    };
    assert!(matches!(
        controller.reserve(identity(&controller, "c")).await,
        Err(ControllerError::Refused(PauseReason::InflightLimit))
    ));
    let (sending_a, sending_b) = tokio::join!(controller.begin_send(&a), controller.begin_send(&b));
    assert_ne!(sending_a.is_ok(), sending_b.is_ok());
    let sent = sending_a.or(sending_b)?;
    assert_eq!(controller.status().await?.inflight.total, 1);
    assert!(matches!(
        controller
            .reserve(identity(&controller, &sent.request_id))
            .await?,
        Admission::Existing(_)
    ));
    let mut conflict = identity(&controller, &sent.request_id);
    conflict.digest = "b".repeat(64);
    assert!(matches!(
        controller.reserve(conflict).await,
        Err(ControllerError::Conflict)
    ));
    assert!(controller.begin_send(&sent).await.is_err());
    controller.mark_running(&sent).await?;
    controller
        .complete(&sent, TerminalEvidence::Succeeded)
        .await?;
    assert_eq!(controller.status().await?.inflight.total, 0);
    assert_eq!(
        controller
            .query(&sent.request_id)
            .await?
            .ok_or("record absent")?
            .state,
        ExecutionState::Succeeded
    );
    Ok(())
}

#[tokio::test]
async fn crash_recovery_never_replays_or_expires_unknown_and_counts_old_generation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let config = configuration(directory.path())?;
    let controller = Controller::open(config.clone(), true).await?;
    healthy(&controller).await?;
    let unsent = reserve(&controller, "unsent").await?;
    let sent = reserve(&controller, "sent").await?;
    controller.begin_send(&sent).await?;
    drop(controller);
    await_owner_release(directory.path()).await?;
    let mut next_config = config;
    next_config.pool.members[0].inference_endpoint = "http://127.0.0.1:9998/v1".into();
    let recovered = Controller::open(next_config, false).await?;
    assert_ne!(recovered.generation(), sent.pool_generation);
    assert_eq!(
        recovered
            .query(&unsent.request_id)
            .await?
            .ok_or("unsent absent")?
            .state,
        ExecutionState::Cancelled
    );
    assert_eq!(
        recovered
            .query(&sent.request_id)
            .await?
            .ok_or("sent absent")?
            .state,
        ExecutionState::Unknown
    );
    assert!(matches!(
        recovered.reserve(identity(&recovered, "sent")).await?,
        Admission::Existing(_)
    ));
    recovered.update_snapshot(snapshot()).await?;
    assert!(matches!(
        recovered.reserve(identity(&recovered, "new")).await,
        Err(ControllerError::Refused(PauseReason::UnknownExecution))
    ));
    assert!(recovered.cancel_before_send("sent").await.is_err());
    assert!(recovered.begin_send(&sent).await.is_err());
    recovered.resume().await?;
    assert_eq!(recovered.status().await?.inflight.unknown, 1);
    Ok(())
}

#[tokio::test]
async fn pause_pressure_expiry_and_health_window_gate_reservation_and_send() -> TestResult {
    let directory = tempfile::tempdir()?;
    let controller = Controller::open(configuration(directory.path())?, true).await?;
    healthy(&controller).await?;
    let reserved = reserve(&controller, "pause").await?;
    controller.pause().await?;
    assert!(matches!(
        controller.begin_send(&reserved).await,
        Err(ControllerError::Refused(PauseReason::ManualPause))
    ));
    assert_eq!(controller.status().await?.inflight.total, 0);
    controller.resume().await?;
    healthy(&controller).await?;
    let sending = reserve(&controller, "running").await?;
    controller.begin_send(&sending).await?;
    let mut pressure = snapshot();
    pressure
        .summary
        .metrics
        .as_mut()
        .ok_or("metrics absent")?
        .waiting_requests = 1;
    assert_eq!(
        controller.update_snapshot(pressure).await?.reason,
        Some(PauseReason::MainPressure)
    );
    assert_eq!(controller.status().await?.inflight.total, 1);
    assert!(
        controller
            .reserve(identity(&controller, "blocked"))
            .await
            .is_err()
    );
    assert_eq!(
        controller.update_snapshot(snapshot()).await?.reason,
        Some(PauseReason::HealthWindow)
    );
    controller
        .complete(&sending, TerminalEvidence::Failed)
        .await?;
    Ok(())
}

#[tokio::test]
async fn retained_source_ages_clock_faults_and_partial_samples_fail_closed() -> TestResult {
    let directory = tempfile::tempdir()?;
    let controller = Controller::open(configuration(directory.path())?, true).await?;
    healthy(&controller).await?;
    let mut old = snapshot();
    old.members[0].freshness.collection_completed_at_ms = Some(old.observed_at_ms - 2500);
    assert_eq!(
        controller.update_snapshot(old).await?.reason,
        Some(PauseReason::TelemetryStale)
    );
    let mut partial = snapshot();
    partial.members[0].freshness.source = SourceFreshness::PartialTimestamps;
    assert_eq!(
        controller.update_snapshot(partial).await?.reason,
        Some(PauseReason::SourceFreshnessUnknown)
    );
    let mut future = snapshot();
    future.observed_at_ms += 10_000;
    assert_eq!(
        controller.update_snapshot(future).await?.reason,
        Some(PauseReason::ClockUntrusted)
    );
    assert!(
        controller
            .reserve(identity(&controller, "blocked"))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn missing_initialized_journal_cannot_reset_occupancy_and_pause_persists() -> TestResult {
    let directory = tempfile::tempdir()?;
    let config = configuration(directory.path())?;
    let controller = Controller::open(config.clone(), true).await?;
    controller.pause().await?;
    drop(controller);
    await_owner_release(directory.path()).await?;
    let reopened = Controller::open(config.clone(), false).await?;
    assert!(reopened.status().await?.paused);
    drop(reopened);
    await_owner_release(directory.path()).await?;
    std::fs::remove_file(directory.path().join("connector.sqlite3"))?;
    assert!(matches!(
        Controller::open(config, true).await,
        Err(ControllerError::StateNotInitialized)
    ));
    Ok(())
}

#[tokio::test]
async fn reconciliation_requires_exact_unknown_revision_and_keeps_id_retained() -> TestResult {
    let directory = tempfile::tempdir()?;
    let controller = Controller::open(configuration(directory.path())?, true).await?;
    healthy(&controller).await?;
    let sent = reserve(&controller, "reconcile").await?;
    controller.begin_send(&sent).await?;
    let unknown = controller.mark_unknown(&sent, "transport_lost").await?;
    let assertion = OperatorAttestation {
        attempt_id: unknown.attempt_id.clone(),
        pool_generation: unknown.pool_generation.clone(),
        expected_revision: unknown.revision,
        expected_state: ExecutionState::Unknown,
        confirmed_not_running: true,
        evidence_reference: "provider-diagnosis/ticket-42".into(),
    };
    let mut stale = assertion.clone();
    stale.expected_revision -= 1;
    assert!(controller.reconcile("reconcile", stale).await.is_err());
    let mut unconfirmed = assertion.clone();
    unconfirmed.confirmed_not_running = false;
    assert!(
        controller
            .reconcile("reconcile", unconfirmed)
            .await
            .is_err()
    );
    assert_eq!(controller.status().await?.inflight.unknown, 1);
    let resolved = controller.reconcile("reconcile", assertion.clone()).await?;
    assert_eq!(resolved.state, ExecutionState::Reconciled);
    assert_eq!(
        resolved.evidence.as_deref(),
        Some("operator_confirmed_stopped")
    );
    assert_eq!(
        resolved.operator_evidence_reference.as_deref(),
        Some("provider-diagnosis/ticket-42")
    );
    assert_eq!(controller.status().await?.inflight.total, 0);
    assert!(controller.reconcile("reconcile", assertion).await.is_err());
    assert!(matches!(
        controller
            .reserve(identity(&controller, "reconcile"))
            .await?,
        Admission::Existing(_)
    ));
    assert!(
        controller
            .complete(&sent, TerminalEvidence::Succeeded)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn expired_budget_cancels_only_unsent_and_full_journal_never_forgets_ids() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut config = configuration(directory.path())?;
    let limits = config.execution.as_mut().ok_or("execution absent")?;
    limits.lease_ms = 100;
    limits.max_records = 2;
    let controller = Controller::open(config, true).await?;
    healthy(&controller).await?;
    let sent = reserve(&controller, "sent").await?;
    controller.begin_send(&sent).await?;
    let unsent = reserve(&controller, "unsent").await?;
    tokio::time::sleep(Duration::from_millis(110)).await;
    assert!(matches!(
        controller.begin_send(&unsent).await,
        Err(ControllerError::Refused(PauseReason::BudgetExpired))
    ));
    assert_eq!(controller.status().await?.inflight.total, 1);
    assert_eq!(controller.status().await?.inflight.sending, 1);
    healthy(&controller).await?;
    assert!(matches!(
        controller.reserve(identity(&controller, "full")).await,
        Err(ControllerError::Refused(PauseReason::JournalFull))
    ));
    assert!(matches!(
        controller.reserve(identity(&controller, "sent")).await?,
        Admission::Existing(_)
    ));
    Ok(())
}

#[tokio::test]
async fn failed_durable_write_fences_every_later_admission_until_recovery() -> TestResult {
    use sea_orm::sqlx::{Connection, SqliteConnection};
    let directory = tempfile::tempdir()?;
    let controller = Controller::open(configuration(directory.path())?, true).await?;
    healthy(&controller).await?;
    let mut fault = SqliteConnection::connect(&format!(
        "sqlite://{}?mode=rw",
        directory.path().join("connector.sqlite3").display()
    ))
    .await?;
    // Simulate persistent storage corruption while keeping the owner alive.
    sea_orm::sqlx::query("DROP TABLE connector_executions")
        .execute(&mut fault)
        .await?;
    assert!(matches!(
        controller.reserve(identity(&controller, "ambiguous")).await,
        Err(ControllerError::Storage)
    ));
    sea_orm::sqlx::query("CREATE TABLE connector_executions (request_id TEXT PRIMARY KEY NOT NULL, payload TEXT NOT NULL)")
        .execute(&mut fault).await?;
    assert!(matches!(
        controller.reserve(identity(&controller, "later")).await,
        Err(ControllerError::Refused(PauseReason::StorageUnavailable))
    ));
    assert!(matches!(
        controller.resume().await,
        Err(ControllerError::Storage)
    ));
    assert_eq!(
        controller.status().await?.budget.reason,
        Some(PauseReason::StorageUnavailable)
    );
    fault.close().await?;
    Ok(())
}

#[tokio::test]
async fn slow_durable_intent_cannot_authorize_transport_after_lease_expiry() -> TestResult {
    use std::{
        future::Future,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::Poll,
    };

    use super::{executor::Executor, request::LocalRequest};

    let hits = Arc::new(AtomicUsize::new(0));
    let upstream_hits = hits.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let upstream = tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move || {
                let hits = upstream_hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({}))
                }
            }),
        );
        axum::serve(listener, app).await
    });
    let directory = tempfile::tempdir()?;
    let mut config = configuration(directory.path())?;
    config.pool.members[0].inference_endpoint = format!("http://{address}/v1");
    config
        .execution
        .as_mut()
        .ok_or("execution absent")?
        .lease_ms = 100;
    let executor = Executor::new(&config).map_err(std::io::Error::other)?;
    let controller = Controller::open(config.clone(), true).await?;
    healthy(&controller).await?;
    let reserved = reserve(&controller, "slow-intent").await?;
    let request: LocalRequest = serde_json::from_value(serde_json::json!({
        "request_id": reserved.request_id,
        "pool_id": reserved.pool_id,
        "pool_generation": reserved.pool_generation,
        "deadline_ms": 1500,
        "request": {
            "model": "test/model",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 10
        }
    }))?;
    let wire = executor
        .build(&config.pool.members[0], &request)
        .map_err(std::io::Error::other)?;
    // The actual persistent store has one connection. Hold it so begin_send
    // passes its initial checks and then waits for durable intent storage.
    let held = controller.hold_journal_connection().await?;
    assert_eq!(controller.update_snapshot(snapshot()).await?.reason, None);
    let (output, _receive) = tokio::sync::mpsc::channel(8);
    let operation = executor.execute(
        controller.clone(),
        reserved.clone(),
        request,
        wire,
        tokio::time::Instant::now() + Duration::from_millis(1500),
        output,
    );
    tokio::pin!(operation);
    std::future::poll_fn(|context| match operation.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(Ok::<(), Box<dyn Error>>(())),
        Poll::Ready(()) => Poll::Ready(Err(
            "execution did not wait for the held journal connection".into(),
        )),
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(120)).await;
    drop(held);
    operation.await;
    upstream.abort();
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let record = controller
        .query(&reserved.request_id)
        .await?
        .ok_or("record absent")?;
    assert_eq!(record.state, ExecutionState::Cancelled);
    assert_eq!(
        record.evidence.as_deref(),
        Some("confirmed_unsent_budget_expired")
    );
    assert_eq!(controller.status().await?.inflight.total, 0);
    Ok(())
}
