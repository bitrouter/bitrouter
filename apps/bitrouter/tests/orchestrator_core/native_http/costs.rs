//! Production settlement evidence imported into durable run ownership.
use super::*;
use bitrouter::cloud::settlement::{SettlementReceipt, SettlementState, SettlementUsage};
use bitrouter::metering::store::MeteringStore;
use bitrouter_sdk::language_model::native_accounting::{NativeCostBasis, NativeCostScope};
use sea_orm::{ActiveModelTrait, Set};

struct TimingHarness {
    inner: crate::Harness,
    entered: tokio::sync::Semaphore,
    resume: tokio::sync::Semaphore,
}

#[async_trait]
impl bitrouter_orchestrator::core::session::HarnessPort for TimingHarness {
    async fn read_artifact(
        &self,
        reference: &bitrouter_orchestrator::core::protocol::ArtifactRef,
        offset: u64,
        max_bytes: u64,
    ) -> std::result::Result<Vec<u8>, bitrouter_orchestrator::core::protocol::CoreError> {
        self.inner.read_artifact(reference, offset, max_bytes).await
    }

    async fn commit(
        &self,
        batch: bitrouter_orchestrator::core::checkpoint::CheckpointBatch,
    ) -> std::result::Result<
        bitrouter_orchestrator::core::checkpoint::CheckpointAck,
        bitrouter_orchestrator::core::protocol::CoreError,
    > {
        let payload = batch.decode(&Limits::default())?;
        if payload.events.iter().any(|event| {
            event.kind == "provider.work.outcome"
                && event.payload.pointer("/work/attempt_index") == Some(&json!(1))
                && event.payload.pointer("/work/work_index") == Some(&json!(0))
        }) {
            self.entered.add_permits(1);
            self.resume
                .acquire()
                .await
                .map_err(|_| {
                    bitrouter_orchestrator::core::protocol::CoreError::rejected(
                        bitrouter_orchestrator::core::protocol::ErrorCode::CheckpointUnavailable,
                        "fixture stopped",
                    )
                })?
                .forget();
        }
        self.inner.commit(batch).await
    }

    async fn send(
        &self,
        message: bitrouter_orchestrator::core::protocol::ServerMessage,
    ) -> std::result::Result<(), bitrouter_orchestrator::core::protocol::CoreError> {
        self.inner.send(message).await
    }
}

#[tokio::test]
async fn provider_work_gate_wait_is_excluded_from_actual_metering_generation_time() -> Result<()> {
    let fixture = Fixture::new(true, true).await?;
    let db = fixture.assembled.db.clone();
    let grant = OwnershipGrant {
        session_id: "cost-session".into(),
        harness_id: "cost-harness".into(),
        core_instance_id: "cost-core".into(),
        execution_epoch: 1,
    };
    let harness = Arc::new(TimingHarness {
        inner: crate::Harness {
            grant: grant.clone(),
            store: Mutex::new(crate::Store::default()),
        },
        entered: tokio::sync::Semaphore::new(0),
        resume: tokio::sync::Semaphore::new(0),
    });
    let session = bind_port(Arc::new(fixture.assembled.app), harness.clone(), grant).await?;
    let mut task = crate::input("priced-task");
    task.model = "resilient".into();
    session
        .start("input", session.head().await.state_revision, task)
        .await?;
    let driver = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), harness.entered.acquire())
        .await??
        .forget();
    assert!(
        fixture
            .healthy
            .received_requests()
            .await
            .context("healthy requests")?
            .is_empty()
    );
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    harness.resume.add_permits(1);
    let done = driver.await??;
    assert_eq!(
        done.run.as_ref().context("run")?.status,
        RunStatus::Completed
    );
    let row = requests::Entity::find()
        .one(&db)
        .await?
        .context("settlement")?;
    assert!(row.latency_ms >= 1200);
    assert!(row.generation_time_ms < 1000);
    let report = &done.root_turn().context("turn")?.steps[0].attempts[1]
        .receipt
        .as_ref()
        .context("receipt")?
        .report;
    assert!(report.elapsed_ms < 1000);
    assert!(done.run.as_ref().context("run")?.active_ms < 1000);
    assert_eq!(row.estimated_charge_micro_usd, 255);
    Ok(())
}

#[tokio::test]
async fn monetary_claims_usage_free_rate_limit_is_unknown_despite_legacy_zero() -> Result<()> {
    let fixture = Fixture::new(false, true).await?;
    for provider in [&fixture.failing, &fixture.healthy] {
        provider.reset().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                "error": {"type":"rate_limit_error", "message":"fixture rate limit"}
            })))
            .mount(provider)
            .await;
    }
    let db = fixture.assembled.db.clone();
    let (session, _) = bind(Arc::new(fixture.assembled.app)).await?;
    let mut task = crate::input("rate-limit-cost");
    task.model = "resilient".into();
    session
        .start("input", session.head().await.state_revision, task)
        .await?;
    let failed = session.drive().await?;
    let run = failed.run.as_ref().context("run")?;
    assert_eq!(run.status, RunStatus::Failed);
    let row = requests::Entity::find()
        .one(&db)
        .await?
        .context("request")?;
    assert_eq!(row.charge_status, "computed");
    assert_eq!(row.estimated_charge_micro_usd, 0);
    let raw: Value = serde_json::from_str(row.raw_usage_json.as_deref().context("raw usage")?)?;
    assert_eq!(raw["error"]["code"], "upstream_rate_limited");
    assert!(raw["usage"].is_null());
    let ledger = &failed.cost_work[&run.run_id];
    assert!(ledger.charges.is_empty());
    assert_eq!(ledger.charge_unknown.len(), 1);
    let attempts: Vec<_> = ledger
        .work
        .values()
        .filter(|work| {
            work.kind
                == bitrouter_orchestrator::core::accounting::work::CostWorkKind::ProviderAttempt
        })
        .collect();
    assert_eq!(attempts.len(), 2);
    assert!(
        attempts
            .iter()
            .all(|work| matches!(work.token_estimate, Some(NativeTokenCost::Unknown { .. })))
    );
    Ok(())
}

async fn bind(app: Arc<bitrouter_sdk::App>) -> Result<(CoreSession, Arc<crate::Harness>)> {
    let grant = OwnershipGrant {
        session_id: "cost-session".into(),
        harness_id: "cost-harness".into(),
        core_instance_id: "cost-core".into(),
        execution_epoch: 1,
    };
    let harness = Arc::new(crate::Harness {
        grant: grant.clone(),
        store: Mutex::new(crate::Store::default()),
    });
    let session = bind_port(app, harness.clone(), grant).await?;
    Ok((session, harness))
}

async fn bind_port(
    app: Arc<bitrouter_sdk::App>,
    harness: Arc<dyn bitrouter_orchestrator::core::session::HarnessPort>,
    grant: OwnershipGrant,
) -> Result<CoreSession> {
    Ok(CoreSession::bind(
        Bind {
            grant,
            durable_head: DurableHead::default(),
            checkpoint: None,
            manifest: HarnessManifest {
                tool_manifest_digest: HarnessManifest::digest(&[])?,
                tools: Vec::new(),
                workspace_id: "cost-workspace".into(),
                workspace_revision: None,
                permission_revision: 1,
                max_tool_output_bytes: 8192,
                artifact_quota_bytes: 1024 * 1024,
                max_artifact_chunk_bytes: 8192,
                required_features: Vec::new(),
            },
            limits: Limits::default(),
        },
        &Capabilities {
            version: 1,
            core_instance_id: "cost-core".into(),
            operations: Vec::new(),
            transports: vec!["in_process".into()],
            unsupported_features: Vec::new(),
            limits: Limits::default(),
            max_sessions: 16,
            max_host_model_attempts: 16,
        },
        app,
        CallerContext::local(),
        harness,
    )
    .await?)
}

#[tokio::test]
async fn monetary_claims_retain_estimate_and_import_late_reconciled_bill_once() -> Result<()> {
    for no_charge in [false, true] {
        let fixture = Fixture::new(true, true).await?;
        let db = fixture.assembled.db.clone();
        let app = Arc::new(fixture.assembled.app);
        let (session, harness) = bind(app.clone()).await?;
        let mut task = crate::input("priced-task");
        task.model = "resilient".into();
        session
            .start("first", session.head().await.state_revision, task.clone())
            .await?;
        let first = session.drive().await?;
        let run_id = first.run.as_ref().context("run")?.run_id.clone();
        let ledger = &first.cost_work[&run_id];
        assert_eq!(first.run.as_ref().context("run")?.model_attempts, 2);
        assert_eq!(ledger.charges.len(), 1);
        let estimate = ledger.charges.values().next().context("estimate")?.clone();
        assert_eq!(estimate.basis, NativeCostBasis::Estimated);
        assert_eq!(estimate.scope, NativeCostScope::ModelTokens);
        assert_eq!(estimate.micro_usd, 255);
        assert_eq!(
            ledger.charge_unknown[&estimate.request_id],
            "charge_not_reconciled"
        );
        let foreign = app
            .native_cost_observations(
                &CallerContext::new("local", "other-owner"),
                std::slice::from_ref(&estimate.request_id),
            )
            .await?;
        assert!(foreign[0].claims.is_empty());
        assert_eq!(
            foreign[0].unknown_reason.as_deref(),
            Some("settlement_unavailable")
        );
        session
            .start("next", session.head().await.state_revision, task)
            .await?;
        let successor = session.snapshot().await;
        let next_run = successor.run.as_ref().context("successor")?.run_id.clone();
        assert_ne!(run_id, next_run);
        let row = requests::Entity::find_by_id(&estimate.request_id)
            .one(&db)
            .await?
            .context("metered row")?;
        let mut pending: requests::ActiveModel = row.into();
        pending.reconciliation_status = Set("pending".into());
        pending.update(&db).await?;
        let receipt = SettlementReceipt {
            request_id: estimate.request_id.clone(),
            state: if no_charge {
                SettlementState::NotCharged
            } else {
                SettlementState::Computed
            },
            provider_id: Some("healthy".into()),
            model_id: Some("served-model".into()),
            usage: SettlementUsage {
                uncached_input_tokens: if no_charge { 0 } else { 60 },
                cache_read_tokens: if no_charge { 0 } else { 30 },
                cache_write_tokens: if no_charge { 0 } else { 10 },
                output_tokens: if no_charge { 0 } else { 3 },
                reasoning_tokens: if no_charge { 0 } else { 2 },
            },
            final_charge_micro_usd: if no_charge { None } else { Some(47) },
        };
        MeteringStore::new(db.clone())
            .apply_authoritative_receipt_charge(&receipt)
            .await?;
        let applied = session.refresh_costs("late-cost", &run_id).await?;
        let late = session.snapshot().await;
        let ledger = &late.cost_work[&run_id];
        assert_eq!(ledger.charges.len(), 3);
        assert!(ledger.charges.values().any(|claim| claim == &estimate));
        for basis in [NativeCostBasis::Reported, NativeCostBasis::Reconciled] {
            let claims: Vec<_> = ledger
                .charges
                .values()
                .filter(|claim| claim.basis == basis)
                .collect();
            assert_eq!(claims.len(), 1);
            assert_eq!(claims[0].micro_usd, if no_charge { 0 } else { 47 });
            assert_eq!(claims[0].bill_id, estimate.bill_id);
        }
        assert!(ledger.charge_unknown.is_empty());
        assert_eq!(late.cost_work[&next_run], successor.cost_work[&next_run]);
        assert_eq!(session.refresh_costs("late-cost", &run_id).await?, applied);
        assert_eq!(session.snapshot().await.cost_work, late.cost_work);
        session.refresh_costs("same-evidence", &run_id).await?;
        assert_eq!(session.snapshot().await.cost_work, late.cost_work);
        assert_eq!(
            fixture
                .healthy
                .received_requests()
                .await
                .context("healthy requests")?
                .len(),
            1
        );
        assert_eq!(
            fixture
                .failing
                .received_requests()
                .await
                .context("failed requests")?
                .len(),
            1
        );
        assert_eq!(requests::Entity::find().all(&db).await?.len(), 1);
        let store = harness.store.lock().await;
        let last = store
            .batches
            .last()
            .context("checkpoint")?
            .decode(&Limits::default())?;
        assert_eq!(last.events[0].run_id.as_deref(), Some(run_id.as_str()));
        let persisted: bitrouter_orchestrator::core::session::SessionSnapshot =
            serde_json::from_value(last.checkpoint.state)?;
        assert_eq!(persisted.cost_work, late.cost_work);
    }
    Ok(())
}

#[tokio::test]
async fn monetary_claims_unknown_usage_and_conflicting_receipts_never_become_free() -> Result<()> {
    let fixture = Fixture::new(false, true).await?;
    let db = fixture.assembled.db.clone();
    let (session, _) = bind(Arc::new(fixture.assembled.app)).await?;
    let mut task = crate::input("unknown-cost");
    task.model = "resilient".into();
    session
        .start("input", session.head().await.state_revision, task)
        .await?;
    let first = session.drive().await?;
    let run_id = first.run.as_ref().context("run")?.run_id.clone();
    assert!(first.cost_work[&run_id].charges.is_empty());
    assert_eq!(first.cost_work[&run_id].charge_unknown.len(), 1);
    let row = requests::Entity::find()
        .one(&db)
        .await?
        .context("request")?;
    let request_id = row.request_id.clone();
    let mut pending: requests::ActiveModel = row.into();
    pending.reconciliation_status = Set("pending".into());
    pending.update(&db).await?;
    let receipt = SettlementReceipt {
        request_id: request_id.clone(),
        state: SettlementState::Computed,
        provider_id: Some("healthy".into()),
        model_id: Some("served-model".into()),
        usage: SettlementUsage {
            uncached_input_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 1,
            reasoning_tokens: 0,
        },
        final_charge_micro_usd: Some(13),
    };
    MeteringStore::new(db.clone())
        .apply_authoritative_receipt_charge(&receipt)
        .await?;
    session.refresh_costs("first-report", &run_id).await?;
    let before = session.snapshot().await;
    let head = session.head().await;
    let row = requests::Entity::find_by_id(&request_id)
        .one(&db)
        .await?
        .context("row")?;
    let mut changed = receipt;
    changed.final_charge_micro_usd = Some(19);
    let mut tampered: requests::ActiveModel = row.into();
    tampered.authoritative_receipt_json = Set(Some(serde_json::to_string(&changed)?));
    tampered.update(&db).await?;
    let conflict = session.refresh_costs("conflict", &run_id).await;
    assert_eq!(
        conflict.err().map(|error| error.code),
        Some(bitrouter_orchestrator::core::protocol::ErrorCode::OperationConflict)
    );
    assert_eq!(session.head().await, head);
    assert_eq!(session.snapshot().await.cost_work, before.cost_work);
    Ok(())
}
