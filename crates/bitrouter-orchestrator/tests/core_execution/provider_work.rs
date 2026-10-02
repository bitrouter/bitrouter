//! Actual HTTP retries, durable sub-work barriers and shared settlement.

use super::*;
use bitrouter_orchestrator::core::accounting::work::{CostWorkKind, CostWorkState};
use bitrouter_sdk::language_model::auth::{AuthApplier, AuthAppliers};
use bitrouter_sdk::language_model::executor::{HttpExecutor, HttpTimeouts};
use bitrouter_sdk::language_model::native_work::NativeProviderWorkKind;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Default)]
struct RetryAuth {
    refreshes: AtomicUsize,
    fail_refresh: AtomicBool,
}

#[async_trait]
impl AuthApplier for RetryAuth {
    fn output_token_limit_support(&self, _: &RoutingTarget) -> Option<bool> {
        Some(true)
    }

    async fn apply(
        &self,
        mut request: reqwest::Request,
        _: &RoutingTarget,
    ) -> bitrouter_sdk::Result<reqwest::Request> {
        let token = if self.refreshes.load(Ordering::SeqCst) == 0 {
            "Bearer stale-private-token"
        } else {
            "Bearer fresh-private-token"
        };
        request.headers_mut().insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static(token),
        );
        Ok(request)
    }

    async fn refresh_after_unauthorized(
        &self,
        _: &RoutingTarget,
        _: Option<&reqwest::header::HeaderValue>,
    ) -> bitrouter_sdk::Result<bool> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        if self.fail_refresh.load(Ordering::SeqCst) {
            Err(bitrouter_sdk::BitrouterError::internal(
                "private-refresh-diagnostic",
            ))
        } else {
            Ok(true)
        }
    }
}

#[derive(Clone, Default)]
struct UsageRecords(Arc<Mutex<Vec<(String, u64, u64)>>>);

#[async_trait]
impl SettlementRecorder for UsageRecords {
    async fn record(&self, ctx: &mut SettlementContext) -> bitrouter_sdk::Result<()> {
        self.0.lock().await.push((
            ctx.request_id.clone(),
            ctx.prompt_tokens,
            ctx.completion_tokens,
        ));
        Ok(())
    }
}

struct WorkHarness {
    inner: Arc<Harness>,
    event: &'static str,
    work_index: u32,
    hold: AtomicBool,
    fail: bool,
    seen: Semaphore,
    resume: Semaphore,
}

impl WorkHarness {
    fn new(event: &'static str, work_index: u32, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Harness::new(None, None)),
            event,
            work_index,
            hold: AtomicBool::new(true),
            fail,
            seen: Semaphore::new(0),
            resume: Semaphore::new(0),
        })
    }
}

#[async_trait]
impl HarnessPort for WorkHarness {
    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        let payload = batch.decode(&Limits::default())?;
        let selected = payload.events.iter().any(|event| {
            let work = if event.kind == "provider.work.outcome" {
                event.payload.get("work")
            } else {
                Some(&event.payload)
            };
            event.kind == self.event
                && work
                    .and_then(|work| work.get("work_index"))
                    .and_then(serde_json::Value::as_u64)
                    == Some(u64::from(self.work_index))
        });
        if selected && self.hold.load(Ordering::SeqCst) {
            self.seen.add_permits(1);
            self.resume
                .acquire()
                .await
                .map_err(|_| {
                    CoreError::rejected(ErrorCode::CheckpointUnavailable, "fixture stopped")
                })?
                .forget();
        }
        if selected && self.fail {
            return Err(CoreError::rejected(
                ErrorCode::CheckpointUnavailable,
                "lost work ACK",
            ));
        }
        self.inner.commit(batch).await
    }

    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        self.inner.send(message).await
    }
}

async fn fixture(
    harness: Arc<dyn HarnessPort>,
    bridge: bool,
) -> Result<(CoreSession, MockServer, Arc<RetryAuth>, UsageRecords), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    let endpoint = if bridge {
        "/v1/responses"
    } else {
        "/v1/chat/completions"
    };
    let provider = if bridge { "openai-codex" } else { "first" };
    Mock::given(method("POST"))
        .and(path(endpoint))
        .and(header("authorization", "Bearer stale-private-token"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"error":{"message":"expired"}})),
        )
        .mount(&server)
        .await;
    // Official wire shapes:
    // https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create
    // https://developers.openai.com/api/docs/guides/streaming-responses
    let successful = if bridge {
        ResponseTemplate::new(200).set_body_raw(format!("data: {}\n\n", json!({
            "type":"response.completed", "response":{
                "id":"fixture-response", "status":"completed", "store":false,
                "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}],
                "usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}
            }
        })), "text/event-stream")
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "id":"fixture-response", "model":"fixture-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}
        }))
    };
    Mock::given(method("POST"))
        .and(path(endpoint))
        .and(header("authorization", "Bearer fresh-private-token"))
        .respond_with(successful)
        .mount(&server)
        .await;
    let auth = Arc::new(RetryAuth::default());
    let records = UsageRecords::default();
    let executor = HttpExecutor::with_dispatch_and_auth(
        HttpTimeouts {
            total: Some(Duration::from_millis(800)),
            ..Default::default()
        },
        Default::default(),
        AuthAppliers::new().with(provider, auth.clone()),
    )?;
    let mut route = target(provider);
    if bridge {
        route.api_protocol = ApiProtocol::Responses;
    }
    route.api_base = format!("{}/v1", server.uri());
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![route]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(Arc::new(executor))
                .native_cost_estimator(Arc::new(accounting::FixtureCost))
                .settlement_recorder(records.clone());
        })
        .build()?;
    Ok((
        bind_app(Arc::new(app), harness).await?,
        server,
        auth,
        records,
    ))
}

#[tokio::test]
async fn provider_work_http_retry_retains_phases_and_one_settlement() -> TestResult {
    for bridge in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let (session, server, auth, records) = fixture(harness.clone(), bridge).await?;
        session.start("input", 1, input()).await?;
        let done = session.drive().await?;
        assert_eq!(done.run.as_ref().ok_or("run")?.status, RunStatus::Completed);
        assert_eq!(done.run.as_ref().ok_or("run")?.model_attempts, 2);
        let attempt = &done.root_turn().ok_or("turn")?.steps[0].attempts[0];
        assert_eq!(attempt.provider_work.len(), 6);
        let phases: Vec<_> = attempt
            .provider_work
            .iter()
            .map(|record| record.work.kind)
            .collect();
        assert_eq!(
            phases,
            vec![
                NativeProviderWorkKind::AuthenticationPreparation,
                NativeProviderWorkKind::Authentication,
                NativeProviderWorkKind::HttpDispatch,
                NativeProviderWorkKind::AuthenticationRefresh,
                NativeProviderWorkKind::Authentication,
                NativeProviderWorkKind::HttpDispatch
            ]
        );
        let statuses: Vec<_> = attempt
            .provider_work
            .iter()
            .filter_map(|record| record.report.as_ref().and_then(|report| report.http_status))
            .collect();
        assert_eq!(statuses, [401, 200]);
        let token_cost = done
            .run
            .as_ref()
            .ok_or("run")?
            .token_accounting
            .as_ref()
            .ok_or("accounting")?;
        assert_eq!(token_cost.known_attempts, 1);
        assert_eq!(token_cost.unknown_attempts, 1);
        assert_eq!(token_cost.pending_attempts(2), 0);
        assert_eq!(token_cost.complete_estimate_micro_usd(2), None);
        assert_eq!(auth.refreshes.load(Ordering::SeqCst), 1);
        let requests = server.received_requests().await.ok_or("requests")?;
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].headers["x-bitrouter-request-id"],
            requests[1].headers["x-bitrouter-request-id"]
        );
        let settled = records.0.lock().await;
        assert_eq!(settled.len(), 1);
        assert_eq!((settled[0].1, settled[0].2), (10, 5));
        assert_eq!(attempt.provider_work[0].work.request_id, settled[0].0);
        let ledger = &done.cost_work[&done.run.as_ref().ok_or("run")?.run_id];
        assert_eq!(
            ledger
                .work
                .values()
                .filter(|work| work.kind == CostWorkKind::HttpDispatch)
                .count(),
            2
        );
        assert!(
            ledger
                .work
                .values()
                .all(|work| work.state == CostWorkState::OutcomeRecorded
                    && !work.unknown_cost_reason.is_empty())
        );
        let serialized = serde_json::to_string(&done)?;
        for private in [
            "stale-private-token",
            "fresh-private-token",
            "do-not-checkpoint-this-key",
        ] {
            assert!(!serialized.contains(private));
        }
        let store = harness.store.lock().await;
        let payload = store
            .batches
            .last()
            .ok_or("checkpoint")?
            .decode(&Limits::default())?;
        let restored: SessionSnapshot = serde_json::from_value(payload.checkpoint.state)?;
        assert_eq!(restored.cost_work, done.cost_work);
    }
    Ok(())
}

#[tokio::test]
async fn provider_work_internal_retry_obeys_shared_attempt_budget() -> TestResult {
    for bridge in [false, true] {
        let (session, server, _, records) =
            fixture(Arc::new(Harness::new(None, None)), bridge).await?;
        let mut task = input();
        task.limits = Some(Limits {
            model_attempts: 1,
            ..Default::default()
        });
        session.start("input", 1, task).await?;
        let done = session.drive().await?;
        let run = done.run.as_ref().ok_or("run")?;
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.model_attempts, 1);
        assert_eq!(server.received_requests().await.ok_or("requests")?.len(), 1);
        assert_eq!(records.0.lock().await.len(), 1);
        assert_eq!(
            run.token_accounting
                .as_ref()
                .ok_or("accounting")?
                .unknown_attempts,
            1
        );
        assert_eq!(
            done.root_turn().ok_or("turn")?.steps[0].attempts[0]
                .provider_work
                .len(),
            5
        );
    }
    Ok(())
}

#[tokio::test]
async fn provider_work_http_intent_ack_precedes_dispatch_and_cancel_fences_retry() -> TestResult {
    for bridge in [false, true] {
        for index in [2, 5] {
            let harness = WorkHarness::new("provider.work.intent", index, false);
            let (session, server, _, _) = fixture(harness.clone(), bridge).await?;
            let accepted = session.start("input", 1, input()).await?;
            let driver = tokio::spawn({
                let session = session.clone();
                async move { session.drive().await }
            });
            tokio::time::timeout(Duration::from_secs(3), harness.seen.acquire())
                .await??
                .forget();
            let sent = usize::from(index == 5);
            assert_eq!(
                server.received_requests().await.ok_or("requests")?.len(),
                sent
            );
            assert_eq!(
                session
                    .snapshot()
                    .await
                    .run
                    .as_ref()
                    .ok_or("run")?
                    .model_attempts,
                1
            );
            let mut cancel = Box::pin(session.cancel_run(
                "cancel",
                session.head().await.state_revision + 1,
                &accepted.assigned_ids["run_id"],
            ));
            assert!(futures::poll!(cancel.as_mut()).is_pending());
            harness.hold.store(false, Ordering::SeqCst);
            harness.resume.add_permits(1);
            cancel.await?;
            let cancelled = driver.await??;
            assert_eq!(
                cancelled.run.as_ref().ok_or("run")?.status,
                RunStatus::Cancelled
            );
            assert_eq!(
                server.received_requests().await.ok_or("requests")?.len(),
                sent
            );
            let attempt = &cancelled.root_turn().ok_or("turn")?.steps[0].attempts[0];
            let last = attempt.provider_work.last().ok_or("work")?;
            assert_eq!(last.work.work_index, index);
            assert!(last.report.is_none());
            let run = cancelled.run.as_ref().ok_or("run")?;
            assert_eq!(
                run.token_accounting
                    .as_ref()
                    .ok_or("accounting")?
                    .pending_attempts(run.model_attempts),
                u32::from(index == 5)
            );
            if index == 2 {
                assert_eq!(attempt.receipt.as_ref().ok_or("receipt")?.report.continuation.input,
                bitrouter_sdk::language_model::native_continuation::NativeContinuationInput::Unknown);
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn provider_work_http_outcome_ack_wait_is_excluded_and_usage_survives_loss() -> TestResult {
    for bridge in [false, true] {
        for fail in [false, true] {
            let harness = WorkHarness::new("provider.work.outcome", 5, fail);
            let (session, server, _, records) = fixture(harness.clone(), bridge).await?;
            session.start("input", 1, input()).await?;
            let driver = tokio::spawn({
                let session = session.clone();
                async move { session.drive().await }
            });
            tokio::time::timeout(Duration::from_secs(3), harness.seen.acquire())
                .await??
                .forget();
            assert_eq!(server.received_requests().await.ok_or("requests")?.len(), 2);
            let held = session.snapshot().await;
            assert!(
                held.root_turn().ok_or("turn")?.steps[0].attempts[0].provider_work[5]
                    .report
                    .is_none()
            );
            // Longer than the actual HTTP total timeout: accepted bodies must have
            // drained before this ACK wait, and this interval must not count as work.
            tokio::time::sleep(Duration::from_millis(1200)).await;
            harness.hold.store(false, Ordering::SeqCst);
            harness.resume.add_permits(1);
            let result = driver.await?;
            assert_eq!(result.is_err(), fail);
            let settled = records.0.lock().await;
            assert_eq!(settled.len(), 1);
            assert_eq!((settled[0].1, settled[0].2), (10, 5));
            let state = session.snapshot().await;
            let run = state.run.as_ref().ok_or("run")?;
            assert!(run.active_ms < 1000);
            if !fail {
                assert_eq!(run.status, RunStatus::Completed);
                assert!(
                    state.root_turn().ok_or("turn")?.steps[0].attempts[0]
                        .receipt
                        .as_ref()
                        .ok_or("receipt")?
                        .report
                        .elapsed_ms
                        < 1000
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn provider_work_failed_auth_refresh_is_retained_without_another_http_call() -> TestResult {
    for bridge in [false, true] {
        let (session, server, auth, _) =
            fixture(Arc::new(Harness::new(None, None)), bridge).await?;
        auth.fail_refresh.store(true, Ordering::SeqCst);
        session.start("input", 1, input()).await?;
        let failed = session.drive().await?;
        assert_eq!(failed.run.as_ref().ok_or("run")?.status, RunStatus::Failed);
        assert_eq!(server.received_requests().await.ok_or("requests")?.len(), 1);
        let record = failed.root_turn().ok_or("turn")?.steps[0].attempts[0]
            .provider_work
            .last()
            .ok_or("work")?;
        assert_eq!(
            record.work.kind,
            NativeProviderWorkKind::AuthenticationRefresh
        );
        assert!(record.report.as_ref().ok_or("report")?.error_code.is_some());
        assert!(!serde_json::to_string(&failed)?.contains("private-refresh-diagnostic"));
    }
    Ok(())
}
