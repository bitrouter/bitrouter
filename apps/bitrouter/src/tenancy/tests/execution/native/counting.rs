//! Actual controlled count -> generation and the same terminal writer/Registry.
use super::*;
use crate::tenancy::receipts::{
    auxiliary::{Snapshot, count::Envelope},
    store as receipts,
};
use bitrouter_ai::{
    core::evidence::NativeInputCount,
    tenancy::{
        execution::{ExecutionReceipt, Provenance},
        identity::ExecutionId,
    },
};
use sea_orm::{
    ConnectionTrait,
    sea_query::{Alias, Expr, Query},
};

#[derive(Default)]
struct CountControl {
    started: Mutex<Vec<String>>,
    reports: Mutex<Vec<NativeInputCountReport>>,
    attempts: Mutex<Vec<NativeAttemptReport>>,
    plans: AtomicUsize,
    rebuilds: AtomicUsize,
    rebuild: bool,
    after_gate: Option<Arc<Gate>>,
    reject_report: bool,
    stream: bool,
    text_parts: AtomicUsize,
}
#[async_trait]
impl NativeExecutionControl for CountControl {
    fn observe_stream(&self) -> bool {
        self.stream
    }
    async fn on_stream_part(&self, _: &str, part: &bitrouter_ai::types::StreamPart) {
        if matches!(part, bitrouter_ai::types::StreamPart::TextDelta { text } if text == "native reply")
        {
            self.text_parts.fetch_add(1, Ordering::SeqCst);
        }
    }
    async fn before_input_count(&self, plan: &NativePlan, _: u32) -> GatewayResult<()> {
        self.started.lock().await.push(plan.request_id.clone());
        Ok(())
    }
    async fn after_input_count(&self, report: NativeInputCountReport) -> GatewayResult<()> {
        self.reports.lock().await.push(report);
        if let Some(gate) = &self.after_gate {
            gate.entered.notify_one();
            gate.release.cancelled().await;
        }
        if self.reject_report {
            return Err(BitrouterError::Forbidden(
                "fixture refused count report".into(),
            ));
        }
        Ok(())
    }
    async fn plan(&self, _plan: NativePlan) -> GatewayResult<NativePlanAdmission> {
        let index = self.plans.fetch_add(1, Ordering::SeqCst);
        if self.rebuild && index == 0 {
            return Err(BitrouterError::Forbidden("fixture rebuild".into()));
        }
        Ok(NativePlanAdmission {
            route_indices: vec![0],
        })
    }
    async fn rebuild_context(&self, plan: &NativePlan) -> GatewayResult<Option<Vec<Message>>> {
        self.rebuilds.fetch_add(1, Ordering::SeqCst);
        // Rebuilds retain complete original messages; they cannot synthesize
        // replacement text or retain as many messages as the original history.
        Ok(self
            .rebuild
            .then(|| plan.prompt.messages.last().cloned().into_iter().collect()))
    }
    async fn before_attempt(&self, _: &str, _: u32) -> GatewayResult<()> {
        Ok(())
    }
    async fn after_attempt(&self, report: NativeAttemptReport) {
        self.attempts.lock().await.push(report);
    }
}
pub(super) fn protocols() -> Vec<bool> {
    if cfg!(feature = "http2") {
        vec![false, true]
    } else {
        vec![false]
    }
}
pub(super) fn client(fixture: &Fixture, h2: bool) -> Result<()> {
    let builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(TIMEOUT);
    #[cfg(feature = "http2")]
    let builder = if h2 {
        builder.http2_prior_knowledge()
    } else {
        builder.http1_only()
    };
    #[cfg(not(feature = "http2"))]
    ensure!(!h2);
    fixture
        .service
        .executor
        .count_client_for_test(builder.build()?)?;
    Ok(())
}
async fn setup(
    provider: &Provider,
    h2: bool,
) -> Result<(Fixture, HeaderMap, CredentialIssued, ResourceRef)> {
    let (mut fixture, route, slot) = provision_protocol(&provider.url, "responses").await?;
    let route = revised_route(
        &mut fixture,
        &route,
        &slot,
        &provider.url,
        &["responses"],
        true,
    )
    .await?;
    client(&fixture, h2)?;
    let (headers, credential) = ready(&fixture, &route).await?;
    Ok((fixture, headers, credential, route))
}
async fn facts(fixture: &Fixture, control: &CountControl) -> Result<(ExecutionReceipt, Envelope)> {
    tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
    let id: ExecutionId = control
        .reports
        .lock()
        .await
        .first()
        .context("count report")?
        .request_id
        .as_str()
        .try_into()?;
    facts_for_id(fixture, &id).await
}
pub(super) async fn facts_for_id(
    fixture: &Fixture,
    id: &ExecutionId,
) -> Result<(ExecutionReceipt, Envelope)> {
    tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
    let receipt = receipts::lookup(&fixture.db, &fixture.installation, &fixture.tenant, id)
        .await?
        .context("durable receipt")?;
    let query = Query::select()
        .from(Alias::new("tenancy_receipt_auxiliary"))
        .column(Alias::new("evidence_json"))
        .and_where(Expr::col(Alias::new("installation_id")).eq(fixture.installation.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(fixture.tenant.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(id.as_str()))
        .to_owned();
    let row = fixture
        .db
        .query_one(fixture.db.get_database_backend().build(&query))
        .await?
        .context("durable count evidence")?;
    let Snapshot::Counts(evidence) =
        serde_json::from_str(&row.try_get::<String>("", "evidence_json")?)?
    else {
        anyhow::bail!("expected count envelope");
    };
    ensure!(evidence.validate());
    Ok((receipt, evidence))
}
pub(super) fn version(h2: bool) -> http::Version {
    if h2 {
        http::Version::HTTP_2
    } else {
        http::Version::HTTP_11
    }
}

#[tokio::test]
async fn real_owned_counts_bind_one_or_two_prompts_without_consuming_model_attempts() -> Result<()>
{
    count_then_generate(false).await
}

#[tokio::test]
async fn real_owned_counts_bind_one_or_two_prompts_to_observed_stream_generation() -> Result<()> {
    count_then_generate(true).await
}

async fn count_then_generate(stream: bool) -> Result<()> {
    for h2 in protocols() {
        for rebuild in [false, true] {
            let mut provider = Provider::start(false).await?;
            let (fixture, ingress, _, _) = setup(&provider, h2).await?;
            let checked = async {
                let control = Arc::new(CountControl {
                    rebuild,
                    stream,
                    ..Default::default()
                });
                let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                    .with_tenancy(fixture.service.clone());
                let mut input = prompt();
                if rebuild {
                    let current = std::mem::take(&mut input.messages);
                    input.messages = vec![
                        Message::text(Role::User, "earlier question"),
                        Message::text(Role::Assistant, "earlier answer"),
                    ];
                    input.messages.extend(current);
                }
                tokio::time::timeout(
                    TIMEOUT,
                    app.execute_native_controlled_with_headers(
                        input,
                        CallerContext::local(),
                        ingress,
                        control.clone(),
                    ),
                )
                .await??;
                let counts = if rebuild { 2 } else { 1 };
                let mut observed = Vec::new();
                for _ in 0..=counts {
                    observed.push(provider.receive().await?);
                }
                ensure!(observed.iter().all(|r| r.version == version(h2)
                    && r.headers["authorization"] == "Bearer owned-upstream-secret"));
                ensure!(
                    observed[..counts]
                        .iter()
                        .all(|r| r.path == "/v1/responses/input_tokens")
                );
                ensure!(
                    observed
                        .iter()
                        .all(
                            |request| request.headers["openai-organization"] == "count-owned-org"
                                && request.headers["openai-project"] == "count-owned-project"
                                && request.headers["x-private"] == "count-private"
                        )
                );
                ensure!(observed[counts].path == "/v1/responses");
                ensure!(observed[counts].body["stream"] == stream);
                ensure!(
                    observed[..counts]
                        .iter()
                        .all(|request| request.body.get("stream").is_none())
                );
                ensure!(control.text_parts.load(Ordering::SeqCst) == usize::from(stream));
                ensure!(observed[counts - 1].body["input"] == observed[counts].body["input"]);
                if rebuild {
                    ensure!(observed[0].body["input"] != observed[1].body["input"]);
                    let original = observed[0].body["input"]
                        .as_array()
                        .context("original input")?;
                    let retained = observed[1].body["input"]
                        .as_array()
                        .context("rebuilt input")?;
                    ensure!(original.len() == 3 && retained.len() == 1);
                    ensure!(retained.first() == original.last());
                }
                ensure!(control.rebuilds.load(Ordering::SeqCst) == usize::from(rebuild));
                ensure!(provider.calls() == counts + 1 && control.attempts.lock().await.len() == 1);
                let (receipt, evidence) = facts(&fixture, &control).await?;
                ensure!(
                    evidence.counts.len() == counts
                        && evidence
                            .counts
                            .iter()
                            .all(|fact| fact.input_tokens == Some(4))
                );
                ensure!(
                    receipt.attempts.len() == 1
                        && receipt.usage.input_tokens == Some(4)
                        && receipt.usage.output_tokens == Some(2)
                );
                ensure!(receipt.usage.provenance == Provenance::Provider);
                let public_and_private = format!(
                    "{}{}",
                    serde_json::to_string(&receipt)?,
                    serde_json::to_string(&evidence)?
                );
                ensure!(
                    [
                        "count-owned-org",
                        "count-owned-project",
                        "count-private",
                        "owned-upstream-secret"
                    ]
                    .iter()
                    .all(|secret| !public_and_private.contains(secret))
                );
                ensure!(control.rebuilds.load(Ordering::SeqCst) == usize::from(rebuild));
                let reports = control.reports.lock().await;
                for (fact, report) in evidence.counts.iter().zip(reports.iter()) {
                    let NativeInputCount::Counted { request_sha256, .. } = &report.outcome else {
                        anyhow::bail!("missing observed count");
                    };
                    ensure!(fact.generation_request.as_ref() == Some(request_sha256));
                }
                Ok(())
            }
            .await;
            finish(&fixture, provider, checked).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn count_rejection_and_unknown_never_rebuild_or_reach_model_even_with_permissive_control()
-> Result<()> {
    use crate::tenancy::receipts::auxiliary::count::Outcome;
    for h2 in protocols() {
        for (status, body, stalled, expected) in [
            (
                StatusCode::UNAUTHORIZED,
                "private diagnostic".to_owned(),
                true,
                Outcome::Rejected,
            ),
            (
                StatusCode::FORBIDDEN,
                "truncated".to_owned(),
                false,
                Outcome::Rejected,
            ),
            (StatusCode::OK, "{".to_owned(), false, Outcome::Unknown),
            (
                StatusCode::OK,
                "x".repeat(16 * 1024 + 1),
                false,
                Outcome::Unknown,
            ),
        ] {
            let mut provider = Provider::start(false).await?;
            *provider
                .state
                .count_reply
                .lock()
                .map_err(|_| anyhow::anyhow!("reply lock"))? = Some((status, body));
            provider
                .state
                .count_stall_body
                .store(stalled, Ordering::Release);
            let (fixture, ingress, _, _) = setup(&provider, h2).await?;
            let checked = async {
                let control = Arc::new(CountControl {
                    rebuild: true,
                    ..Default::default()
                });
                let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                    .with_tenancy(fixture.service.clone());
                let failure = tokio::time::timeout(
                    TIMEOUT,
                    app.execute_native_controlled_with_headers(
                        prompt(),
                        CallerContext::local(),
                        ingress,
                        control.clone(),
                    ),
                )
                .await?;
                ensure!(failure.is_err());
                let observed = provider.receive().await?;
                ensure!(
                    observed.path == "/v1/responses/input_tokens"
                        && observed.version == version(h2)
                );
                ensure!(provider.calls() == 1 && control.attempts.lock().await.is_empty());
                ensure!(
                    control.plans.load(Ordering::SeqCst) == 0
                        && control.rebuilds.load(Ordering::SeqCst) == 0
                );
                let (receipt, evidence) = facts(&fixture, &control).await?;
                ensure!(evidence.counts.len() == 1 && evidence.counts[0].outcome == expected);
                ensure!(evidence.counts[0].http_status == Some(status.as_u16()));
                ensure!(
                    receipt.attempts.is_empty()
                        && receipt.usage.provenance == Provenance::Unknown
                        && receipt.usage.input_tokens.is_none()
                );
                let inventory = receipts::inventory(&fixture.db, &fixture.installation).await?;
                ensure!(inventory.unknown.len() == usize::from(expected == Outcome::Unknown));
                Ok(())
            }
            .await;
            finish(&fixture, provider, checked).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn known_count_survives_report_rejection_and_live_authority_changes_before_generation()
-> Result<()> {
    for change in ["report", "revoke", "suspend", "scope"] {
        let provider = Provider::start(false).await?;
        let (fixture, ingress, credential, route) = setup(&provider, false).await?;
        let gate = Arc::new(Gate::default());
        let release = gate.release.clone().drop_guard();
        let checked = async {
            let control = Arc::new(CountControl {
                after_gate: Some(gate.clone()),
                reject_report: change == "report",
                ..Default::default()
            });
            let observed = control.clone();
            let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone());
            let call = AbortOnDropHandle::new(tokio::spawn(async move {
                app.execute_native_controlled_with_headers(
                    prompt(),
                    CallerContext::local(),
                    ingress,
                    control,
                )
                .await
            }));
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            if change != "report" {
                change_authority(&fixture, &credential, &route, change).await?;
            }
            gate.release.cancel();
            ensure!(tokio::time::timeout(TIMEOUT, call).await??.is_err());
            let (receipt, evidence) = facts(&fixture, &observed).await?;
            ensure!(evidence.counts[0].input_tokens == Some(4) && receipt.attempts.is_empty());
            ensure!(provider.calls() == 1 && observed.attempts.lock().await.is_empty());
            ensure!(
                receipts::inventory(&fixture.db, &fixture.installation)
                    .await?
                    .unknown
                    .is_empty()
            );
            Ok(())
        }
        .await;
        drop(release);
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}

#[tokio::test]
async fn count_and_model_terminal_stay_owned_through_actual_commit_and_caller_loss() -> Result<()> {
    use crate::tenancy::transaction::Phase;
    for h2 in protocols() {
        let provider = Provider::start(false).await?;
        let (fixture, ingress, _, _) = setup(&provider, h2).await?;
        let gate = fixture.service.cleanup.hold(Phase::Committing);
        let checked = async {
            let control = Arc::new(CountControl::default());
            let observed = control.clone();
            let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone());
            let call = AbortOnDropHandle::new(tokio::spawn(async move {
                app.execute_native_controlled_with_headers(
                    prompt(),
                    CallerContext::local(),
                    ingress,
                    control,
                )
                .await
            }));
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            ensure!(provider.calls() == 2 && !call.is_finished());
            call.abort();
            ensure!(call.await.is_err_and(|error| error.is_cancelled()));
            let mut shutdown = Box::pin(fixture.service.shutdown());
            ensure!(futures::poll!(&mut shutdown).is_pending());
            let mut drained = Box::pin(fixture.service.admission.wait_drained());
            ensure!(futures::poll!(&mut drained).is_pending());
            fixture.service.cleanup.release_gate();
            tokio::time::timeout(TIMEOUT, shutdown).await??;
            tokio::time::timeout(TIMEOUT, drained).await?;
            let (receipt, counts) = facts(&fixture, &observed).await?;
            ensure!(
                receipt.attempts.len() == 1
                    && counts.counts.len() == 1
                    && counts.counts[0].input_tokens == Some(4)
            );
            ensure!(
                receipts::inventory(&fixture.db, &fixture.installation)
                    .await?
                    .rows
                    == 1
            );
            Ok(())
        }
        .await;
        fixture.service.cleanup.release_gate();
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}

#[tokio::test]
async fn count_credential_wait_rechecks_revoke_before_any_auxiliary_or_model_send() -> Result<()> {
    let provider = Provider::start(false).await?;
    let (fixture, ingress, credential, route) = setup(&provider, false).await?;
    let gate = fixture.service.hold_receipt_credential_read_for_test()?;
    let release = gate.release.clone().drop_guard();
    let checked = async {
        let control = Arc::new(CountControl::default());
        let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone());
        let call = AbortOnDropHandle::new(tokio::spawn(async move {
            app.execute_native_controlled_with_headers(
                prompt(),
                CallerContext::local(),
                ingress,
                control,
            )
            .await
        }));
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        change_authority(&fixture, &credential, &route, "revoke").await?;
        gate.release.cancel();
        ensure!(tokio::time::timeout(TIMEOUT, call).await??.is_err());
        ensure!(provider.calls() == 0);
        let id = gate
            .execution
            .get()
            .context("observed admitted execution")?;
        let (receipt, counts) = facts_for_id(&fixture, id).await?;
        ensure!(
            receipt.attempts.is_empty()
                && counts.counts.len() == 1
                && counts.counts[0].outcome
                    == crate::tenancy::receipts::auxiliary::count::Outcome::NotSent
        );
        Ok(())
    }
    .await;
    drop(release);
    finish(&fixture, provider, checked).await
}

// The test entry lives inside execution::tests so the existing native admission
// stays private. This helper consumes its real admission; it constructs no authority.
pub(in crate::tenancy) async fn check_missing_public_count_extension<F, Fut>(admit: F) -> Result<()>
where
    F: FnOnce(Service, crate::tenancy::execution::Submission, String) -> Fut,
    Fut: std::future::Future<
            Output = std::result::Result<
                Arc<crate::tenancy::execution::TenantExecution>,
                crate::tenancy::error::Failure,
            >,
        >,
{
    use crate::gateway::pipeline::{native::NativeCountedRequests, types::PipelineRequest};
    let provider = Provider::start(false).await?;
    let (fixture, ingress, _, _) = setup(&provider, false).await?;
    let gate = fixture.service.hold_receipt_credential_read_for_test()?;
    let release = gate.release.clone().drop_guard();
    let checked = async {
        let execution = admit(
            fixture.service.clone(),
            fixture.service.submission(&ingress).map_err(failure)?,
            MODEL.to_owned(),
        )
        .await
        .map_err(failure)?;
        ensure!(
            execution
                .count_required()
                .map_err(|code| anyhow::anyhow!("native count requirement: {code:?}"))?,
            "the real native admission must retain its count requirement"
        );
        let id = execution.id().clone();
        let pipeline = execution.pipeline()?;
        let mut request = PipelineRequest::new(MODEL, execution.caller(), prompt());
        request.headers = ingress;
        request.request_id = id.to_string();
        execution.attach(pipeline.clone(), &mut request)?;
        // The ordinary execution entry never installs NativeCountedRequests or
        // the controlled counter. The retained native count requirement remains.
        ensure!(
            request
                .extensions
                .get::<Arc<NativeCountedRequests>>()
                .is_none()
        );
        let call = AbortOnDropHandle::new(tokio::spawn(pipeline.execute_detached(request)));
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        ensure!(gate.execution.get() == Some(&id));
        ensure!(!call.is_finished() && provider.calls() == 0);
        // An earlier ingress/routing refusal cannot satisfy this test: the real
        // executor has reached its retained credential read. No authority changes.
        gate.release.cancel();
        let rejected = tokio::time::timeout(TIMEOUT, call).await??;
        ensure!(matches!(rejected, Err(BitrouterError::Forbidden(_))));
        let rejected = execution
            .failure()
            .context("private count guard rejection")?;
        ensure!(
            rejected.0.code == ErrorCode::Forbidden
                && rejected.0.dispatch_state
                    == bitrouter_ai::tenancy::operation::DispatchState::NotStarted
        );
        drop(execution);
        tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
        let receipt = receipts::lookup(&fixture.db, &fixture.installation, &fixture.tenant, &id)
            .await?
            .context("guard failure receipt")?;
        ensure!(provider.calls() == 0 && receipt.attempts.is_empty());
        Ok(())
    }
    .await;
    drop(release);
    finish(&fixture, provider, checked).await
}

#[cfg(unix)]
mod custody;

#[tokio::test]
async fn unsupported_count_continuation_is_fenced_before_host_callbacks_or_provider_io()
-> Result<()> {
    for key in ["previous_response_id", "conversation"] {
        let provider = Provider::start(false).await?;
        let (fixture, ingress, _, _) = setup(&provider, false).await?;
        let checked = async {
            let control = Arc::new(Control::default());
            let mut input = prompt();
            input
                .params
                .extra
                .insert(key.into(), json!("foreign-provider-state"));
            let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone());
            let error = tokio::time::timeout(
                TIMEOUT,
                app.execute_native_controlled_with_headers(
                    input,
                    CallerContext::local(),
                    ingress,
                    control.clone(),
                ),
            )
            .await?
            .err()
            .context("required profile fence")?;
            ensure!(matches!(error, BitrouterError::Forbidden(_)));
            ensure!(provider.calls() == 0 && control.callbacks.load(Ordering::SeqCst) == 0);
            tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
            ensure!(
                receipts::inventory(&fixture.db, &fixture.installation)
                    .await?
                    .rows
                    == 1
            );
            Ok(())
        }
        .await;
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}

#[tokio::test]
async fn caller_loss_during_real_count_handshake_holds_admission_until_producer_and_receipt_settle()
-> Result<()> {
    for h2 in protocols() {
        let mut provider = Provider::start(true).await?;
        let (fixture, ingress, _, _) = setup(&provider, h2).await?;
        let gate = Arc::new(Gate::default());
        let release_callback = gate.release.clone().drop_guard();
        let checked = async {
            let control = Arc::new(CountControl {
                after_gate: Some(gate.clone()),
                ..Default::default()
            });
            let observed = control.clone();
            let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone());
            let caller = AbortOnDropHandle::new(tokio::spawn(async move {
                app.execute_native_controlled_with_headers(
                    prompt(),
                    CallerContext::local(),
                    ingress,
                    control,
                )
                .await
            }));
            let accepted = provider.receive().await?;
            ensure!(
                accepted.path == "/v1/responses/input_tokens" && accepted.version == version(h2)
            );
            let id: ExecutionId = observed
                .started
                .lock()
                .await
                .first()
                .context("count intent callback")?
                .as_str()
                .try_into()?;
            caller.abort();
            ensure!(caller.await.is_err_and(|error| error.is_cancelled()));
            // Cancellation stops the count socket, but its unavailable report
            // is still owed to the same external callback before custody ends.
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            {
                let reports = observed.reports.lock().await;
                ensure!(
                    reports.len() == 1
                        && matches!(reports[0].outcome, NativeInputCount::Unavailable { .. })
                );
            }
            ensure!(!provider.state.release.is_cancelled());
            let receipt_gate = fixture
                .service
                .cleanup
                .hold(crate::tenancy::transaction::Phase::Committing);
            let mut shutdown = Box::pin(fixture.service.shutdown());
            ensure!(futures::poll!(&mut shutdown).is_pending());
            let mut drained = Box::pin(fixture.service.admission.wait_drained());
            ensure!(futures::poll!(&mut drained).is_pending());
            gate.release.cancel();
            tokio::time::timeout(TIMEOUT, receipt_gate.entered.notified()).await?;
            ensure!(
                futures::poll!(&mut drained).is_pending(),
                "callback ACK released the lease before receipt COMMIT"
            );
            fixture.service.cleanup.release_gate();
            tokio::time::timeout(TIMEOUT, shutdown).await??;
            tokio::time::timeout(TIMEOUT, drained).await?;
            let (receipt, counts) = facts_for_id(&fixture, &id).await?;
            ensure!(counts.counts.len() == 1);
            let count = &counts.counts[0];
            ensure!(
                count.outcome == crate::tenancy::receipts::auxiliary::count::Outcome::Unknown
                    && count.input_tokens.is_none()
                    && count.http_status.is_none()
                    && count.generation_request.is_some()
                    && count.count_request.is_some()
            );
            ensure!(receipt.attempts.is_empty() && provider.calls() == 1);
            ensure!(receipt.outcome != bitrouter_ai::tenancy::execution::Outcome::Completed);
            ensure!(
                receipts::inventory(&fixture.db, &fixture.installation)
                    .await?
                    .unknown
                    .len()
                    == 1
            );
            ensure!(
                !provider.state.release.is_cancelled(),
                "test released withheld count headers"
            );
            Ok(())
        }
        .await;
        drop(release_callback);
        fixture.service.cleanup.release_gate();
        provider.state.release.cancel();
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}
