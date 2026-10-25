//! Exercise canonical tenant ingress through the public App boundary.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use bitrouter_ai::{
    core::evidence::NativeAttemptReport,
    tenancy::{
        authority::{BindingPut, CredentialIssued, GrantScope, TenantPut, TenantRole, TenantState},
        configuration::{Limits, ResourceRef},
        operation::{ErrorCode, Mutation},
    },
    types::{ApiProtocol, Content, Message, Prompt, Role},
};
use futures::FutureExt;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use super::{Fixture, MODEL, draft, expiry, failure, headers, provision_protocol, result};
use crate::{
    config::routing_table::ConfigRoutingTable,
    gateway::{
        assembly::{App, PromptTransform},
        caller::CallerContext,
        error::{BitrouterError, Result as GatewayResult},
        model::decision::{
            DecisionExecutor, DecisionRuntime,
            types::{DecisionError, DecisionRequest, DecisionResponse},
        },
        pipeline::{
            native::{
                NativeExecutionControl, NativeInputCountReport, NativePlan, NativePlanAdmission,
            },
            native_accounting::{NativeCostObservation, NativeCostSource},
            native_preparation::NativePreparationWork,
            native_work::{NativeProviderWork, NativeProviderWorkKind, NativeProviderWorkReport},
        },
    },
    tenancy::{Service, actor::OperationLimits, command::Command, resources::Resources, store},
};

const TIMEOUT: Duration = Duration::from_secs(10);

mod continuation;
mod limits;

struct Observed {
    path: String,
    headers: HeaderMap,
    body: Value,
}

#[derive(Clone)]
struct ProviderState {
    received: mpsc::Sender<Observed>,
    calls: Arc<AtomicUsize>,
    release: CancellationToken,
    stop: CancellationToken,
    empty_choices: Arc<std::sync::atomic::AtomicBool>,
}

async fn provider(
    State(state): State<ProviderState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    let path = uri.path().to_owned();
    if state
        .received
        .try_send(Observed {
            path: path.clone(),
            headers,
            body,
        })
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    tokio::select! {
        _ = state.release.cancelled() => {}
        _ = state.stop.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
    // Real wire replies, including response identity, terminal state and usage.
    // https://platform.openai.com/docs/api-reference/chat/object
    // https://platform.openai.com/docs/api-reference/responses/object
    // https://platform.claude.com/docs/en/api/messages
    let mut reply = match path.as_str() {
        "/v1/chat/completions" => json!({
            "id":"chatcmpl-native", "object":"chat.completion", "created":0,
            "model":"model-a", "choices":[{"index":0,"message":{"role":"assistant",
            "content":"native reply"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}
        }),
        "/v1/messages" => json!({
            "id":"msg-native", "type":"message", "role":"assistant", "model":"model-a",
            "content":[{"type":"text","text":"native reply"}],
            "stop_reason":"end_turn", "stop_sequence":null,
            "usage":{"input_tokens":4,"output_tokens":2}
        }),
        "/v1/responses" => json!({
            "id":"resp-native", "object":"response", "status":"completed", "model":"model-a",
            "output":[{"id":"msg-native","type":"message","role":"assistant",
            "status":"completed","content":[{"type":"output_text","text":"native reply",
            "annotations":[]}]}], "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}
        }),
        "/v1/responses/input_tokens" => json!({"object":"response.input_tokens","input_tokens":4}),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    if path == "/v1/chat/completions" && state.empty_choices.load(Ordering::SeqCst) {
        reply["choices"] = json!([]);
    }
    (StatusCode::OK, Json(reply)).into_response()
}

struct Provider {
    url: String,
    received: mpsc::Receiver<Observed>,
    state: ProviderState,
    task: Option<AbortOnDropHandle<std::io::Result<()>>>,
}

impl Provider {
    async fn start(held: bool) -> Result<Self> {
        let (received, observed) = mpsc::channel(16);
        let state = ProviderState {
            received,
            calls: Arc::new(AtomicUsize::new(0)),
            release: CancellationToken::new(),
            stop: CancellationToken::new(),
            empty_choices: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        if !held {
            state.release.cancel();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let router = Router::new()
            .route("/v1/chat/completions", post(provider))
            .route("/v1/messages", post(provider))
            .route("/v1/responses", post(provider))
            .route("/v1/responses/input_tokens", post(provider))
            .with_state(state.clone());
        let stop = state.stop.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
        }));
        Ok(Self {
            url,
            received: observed,
            state,
            task: Some(task),
        })
    }

    fn calls(&self) -> usize {
        self.state.calls.load(Ordering::SeqCst)
    }

    async fn receive(&mut self) -> Result<Observed> {
        tokio::time::timeout(TIMEOUT, self.received.recv())
            .await?
            .context("provider did not receive a request")
    }

    async fn finish(mut self) -> Result<()> {
        self.state.release.cancel();
        self.state.stop.cancel();
        let task = self.task.take().context("provider already joined")?;
        tokio::time::timeout(TIMEOUT, task).await???;
        Ok(())
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.state.release.cancel();
        self.state.stop.cancel();
        // AbortOnDropHandle also bounds cleanup on failed assertions/timeouts.
    }
}

#[derive(Default)]
struct HostSpies {
    transforms: AtomicUsize,
    reads: AtomicUsize,
    decisions: AtomicUsize,
}

impl PromptTransform for HostSpies {
    fn apply(&self, prompt: &mut Prompt) {
        self.transforms.fetch_add(1, Ordering::SeqCst);
        prompt.model = "ambient:model-a".into();
    }
}

#[async_trait]
impl NativeCostSource for HostSpies {
    async fn read(
        &self,
        _caller: &CallerContext,
        _ids: &[String],
    ) -> GatewayResult<Vec<NativeCostObservation>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        // Fixed fixture evidence; caller-supplied identity never forms a query.
        Ok(vec![NativeCostObservation {
            request_id: "legacy-request".into(),
            claims: Vec::new(),
            unknown_reason: Some("legacy-source-was-read".into()),
        }])
    }
}

#[async_trait]
impl DecisionExecutor for HostSpies {
    async fn execute(
        &self,
        _request: &DecisionRequest,
        _cancel: &CancellationToken,
    ) -> std::result::Result<DecisionResponse, DecisionError> {
        self.decisions.fetch_add(1, Ordering::SeqCst);
        Err(DecisionError::invalid_request("unexpected host decision"))
    }
}

fn host_app(fixture: &Fixture, url: &str, spies: Arc<HostSpies>) -> Result<App> {
    let config = crate::config::parse_resolved(&serde_json::to_string(&json!({
        "inherit_defaults":false,
        "providers":{"ambient":{"api_base":url,"api_key":"ambient-secret",
            "api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model-a"}]}}
    }))?)?;
    Ok(App::builder()
        .prompt_transform(spies.clone())
        .decision_model(DecisionRuntime {
            model: "ambient-decisions".into(),
            executor: spies.clone(),
            policy: Default::default(),
            pricing: None,
        })
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(ConfigRoutingTable::from_config(config)))
                .executor(fixture.service.executor.clone())
                .native_cost_source(spies);
        })
        .build()?)
}

fn prompt() -> Prompt {
    Prompt {
        model: MODEL.into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "hello")],
        tools: Vec::new(),
        params: Default::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hold {
    Plan,
    AuthenticationOutcome,
    Dispatch,
    Outcome,
}

#[derive(Default)]
struct Gate {
    entered: Notify,
    release: CancellationToken,
}

#[derive(Default)]
struct Control {
    callbacks: AtomicUsize,
    routing_commits: AtomicUsize,
    counts: AtomicUsize,
    plans: Mutex<Vec<NativePlan>>,
    attempts: Mutex<Vec<NativeAttemptReport>>,
    work: Mutex<Vec<NativeProviderWorkReport>>,
    hold: Option<(Hold, Arc<Gate>)>,
    reject_plan: bool,
}

impl Control {
    async fn checkpoint(&self, stage: Hold) {
        if let Some((held, gate)) = &self.hold
            && *held == stage
        {
            gate.entered.notify_one();
            gate.release.cancelled().await;
        }
    }

    fn called(&self) {
        self.callbacks.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl NativeExecutionControl for Control {
    async fn prepare_routing(
        &self,
        _prompt: &Prompt,
    ) -> GatewayResult<Option<crate::gateway::routing::preparation::Prepared>> {
        self.called();
        Ok(None)
    }

    async fn commit_routing(
        &self,
        _plan: &crate::gateway::routing::plan::Plan,
    ) -> GatewayResult<()> {
        self.called();
        self.routing_commits.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn before_preparation_work(&self, _work: &NativePreparationWork) -> GatewayResult<()> {
        self.called();
        Ok(())
    }

    async fn before_input_count(&self, _plan: &NativePlan, _index: u32) -> GatewayResult<()> {
        self.called();
        self.counts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn after_input_count(&self, _report: NativeInputCountReport) -> GatewayResult<()> {
        self.called();
        self.counts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn plan(&self, plan: NativePlan) -> GatewayResult<NativePlanAdmission> {
        self.called();
        let indices = (0..plan.routes.len())
            .map(|index| {
                u32::try_from(index).map_err(|_| BitrouterError::internal("fixture route count"))
            })
            .collect::<GatewayResult<Vec<_>>>()?;
        self.plans.lock().await.push(plan);
        self.checkpoint(Hold::Plan).await;
        if self.reject_plan {
            return Err(BitrouterError::Forbidden("fixture rejected plan".into()));
        }
        Ok(NativePlanAdmission {
            route_indices: indices,
        })
    }

    async fn before_attempt(&self, _id: &str, _index: u32) -> GatewayResult<()> {
        self.called();
        Ok(())
    }

    async fn before_provider_work(&self, work: &NativeProviderWork) -> GatewayResult<()> {
        self.called();
        if work.kind == NativeProviderWorkKind::HttpDispatch {
            self.checkpoint(Hold::Dispatch).await;
        }
        Ok(())
    }

    async fn after_provider_work(&self, report: NativeProviderWorkReport) {
        self.called();
        let kind = report.work.kind;
        self.work.lock().await.push(report);
        if kind == NativeProviderWorkKind::Authentication {
            self.checkpoint(Hold::AuthenticationOutcome).await;
        }
    }

    async fn after_attempt(&self, report: NativeAttemptReport) {
        self.called();
        self.attempts.lock().await.push(report);
        self.checkpoint(Hold::Outcome).await;
    }
}

async fn ready(fixture: &Fixture, route: &ResourceRef) -> Result<(HeaderMap, CredentialIssued)> {
    let revision = draft(fixture, route, "native-draft", 25).await?;
    let credential = fixture.issue("native-execute").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let ingress = headers(
        fixture,
        credential.secret.as_deref().context("secret")?,
        &revision.revision_id,
    )?;
    Ok((ingress, credential))
}

async fn finish(fixture: &Fixture, provider: Provider, checked: Result<()>) -> Result<()> {
    provider.state.release.cancel();
    let shutdown = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
    let stopped = provider.finish().await;
    checked?;
    shutdown.context("tenant cleanup timed out")??;
    stopped
}

#[tokio::test]
async fn canonical_calls_preserve_literal_route_owned_auth_defaults_and_execution_ids() -> Result<()>
{
    for (protocol, wire, path, response_id, token_field) in [
        (
            "chat_completions",
            ApiProtocol::ChatCompletions,
            "/v1/chat/completions",
            "chatcmpl-native",
            "max_tokens",
        ),
        (
            "messages",
            ApiProtocol::Messages,
            "/v1/messages",
            "msg-native",
            "max_tokens",
        ),
        (
            "responses",
            ApiProtocol::Responses,
            "/v1/responses",
            "resp-native",
            "max_output_tokens",
        ),
    ] {
        let mut provider = Provider::start(false).await?;
        let (mut fixture, route, slot) = provision_protocol(&provider.url, protocol).await?;
        let checked = async {
            // Both wire candidates are approved, but canonical native input
            // must select the source's first protocol, even with a forged hint.
            let route = if wire == ApiProtocol::Messages {
                revised_route(
                    &mut fixture,
                    &route,
                    &slot,
                    &provider.url,
                    &["messages", "chat_completions"],
                    false,
                )
                .await?
            } else {
                route
            };
            let (mut ingress, _) = ready(&fixture, &route).await?;
            for (name, value) in [
                ("x-bitrouter-inbound-protocol", "chat_completions"),
                ("x-request-id", "same-correlation"),
                ("x-agent-session-id", "forged-session"),
                ("x-workflow-id", "forged-workflow"),
                ("x-parent-request-id", "forged-parent"),
            ] {
                ingress.insert(name, value.parse()?);
            }
            let spies = Arc::new(HostSpies::default());
            let app = host_app(&fixture, &provider.url, spies.clone())?
                .with_tenancy(fixture.service.clone());
            let control = Arc::new(Control::default());
            let mut ids = Vec::new();
            for (caller, tokens, temperature) in [
                (
                    CallerContext::new("forged-key", "other-tenant-principal"),
                    25,
                    0.0,
                ),
                (CallerContext::local_launch("forged-launch"), 13, 0.4),
            ] {
                let mut input = prompt();
                if tokens == 13 {
                    input.params.max_tokens = Some(tokens);
                    input.params.temperature = Some(temperature);
                }
                let response = tokio::time::timeout(
                    TIMEOUT,
                    app.execute_native_controlled_with_headers(
                        input,
                        caller,
                        ingress.clone(),
                        control.clone(),
                    ),
                )
                .await??;
                let output = response
                    .result
                    .generation()
                    .context("native generation output")?;
                if wire == ApiProtocol::Responses {
                    // Managed native turns hide raw provider continuation
                    // handles; only an owned sealed artifact may expose reuse.
                    ensure!(output.response_id.is_none());
                } else {
                    ensure!(output.response_id.as_deref() == Some(response_id));
                }
                ensure!(output.content.iter().any(
                    |part| matches!(part, Content::Text { text, .. } if text == "native reply")
                ));
                let usage = output.usage.as_ref().context("provider usage")?;
                ensure!(usage.prompt_tokens == 4 && usage.completion_tokens == 2);
                let observed = provider.receive().await?;
                ensure!(observed.path == path && observed.body["model"] == "model-a");
                ensure!(
                    observed.body[token_field] == tokens
                        && observed.body["temperature"] == temperature
                );
                for other in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
                    if other != token_field {
                        ensure!(
                            observed.body.get(other).is_none(),
                            "conflicting token default survived"
                        );
                    }
                }
                let (auth_name, auth_value) = if wire == ApiProtocol::Messages {
                    ("x-api-key", "owned-upstream-secret")
                } else {
                    ("authorization", "Bearer owned-upstream-secret")
                };
                ensure!(
                    observed
                        .headers
                        .get(auth_name)
                        .is_some_and(|value| value == auth_value)
                );
                ensure!(
                    !observed
                        .headers
                        .contains_key("bitrouter-execution-revision")
                );
                let plans = control.plans.lock().await;
                let plan = plans.last().context("native plan")?;
                ensure!(
                    plan.request_id == response.request_id && plan.request_id != "same-correlation"
                );
                ensure!(plan.original_model == MODEL);
                ensure!(plan.effective_model == "bitrouter/owned");
                ensure!(plan.prompt.model == plan.effective_model);
                ensure!(
                    plan.prompt.params.chat_token_limit_field.is_none(),
                    "canonical input acquired wire provenance"
                );
                ensure!(plan.routes.len() == 1 && plan.routes[0].protocol == wire);
                ensure!(plan.routes[0].provider == "mock" && plan.routes[0].model == "model-a");
                ids.push(response.request_id);
            }
            ensure!(ids.len() == 2 && ids[0] != ids[1]);
            ensure!(provider.calls() == 2 && control.attempts.lock().await.len() == 2);
            let work = control.work.lock().await;
            let sends: Vec<_> = work
                .iter()
                .filter(|report| report.work.kind == NativeProviderWorkKind::HttpDispatch)
                .collect();
            ensure!(sends.len() == 2);
            for (report, id) in sends.iter().zip(&ids) {
                ensure!(
                    report.work.request_id == *id
                        && report.http_status == Some(200)
                        && report.error_code.is_none()
                );
            }
            ensure!(
                spies.transforms.load(Ordering::SeqCst) == 0
                    && spies.decisions.load(Ordering::SeqCst) == 0
            );
            Ok(())
        }
        .await;
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}

#[tokio::test]
async fn invalid_native_ingress_never_reaches_control_provider_or_admission() -> Result<()> {
    let provider = Provider::start(false).await?;
    let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
    let checked = async {
        let (ingress, _) = ready(&fixture, &route).await?;
        let spies = Arc::new(HostSpies::default());
        let app =
            host_app(&fixture, &provider.url, spies.clone())?.with_tenancy(fixture.service.clone());
        // Occupy admission first: an ingress-bound check must return instead of queueing.
        let limits = Limits {
            active_executions: 4,
            queued_work: 4,
            learning_jobs: 0,
        };
        let mut leases = Vec::new();
        for _ in 0..4 {
            leases.push(
                fixture
                    .service
                    .admission
                    .acquire(&fixture.tenant, &limits)
                    .await
                    .map_err(|code| anyhow::anyhow!("lease: {code:?}"))?,
            );
        }
        for case in [
            "missing-token",
            "duplicate-token",
            "missing-binding",
            "duplicate-binding",
            "malformed-binding",
            "wrong-instance",
            "header-bytes",
            "header-values",
            "prompt-bytes",
        ] {
            let mut sent = ingress.clone();
            let mut input = prompt();
            let expected = match case {
                "missing-token" => {
                    sent.remove("authorization");
                    StatusCode::UNAUTHORIZED
                }
                "duplicate-token" => {
                    sent.append("authorization", ingress["authorization"].clone());
                    StatusCode::BAD_REQUEST
                }
                "missing-binding" => {
                    sent.remove("bitrouter-execution-revision");
                    StatusCode::BAD_REQUEST
                }
                "duplicate-binding" => {
                    sent.append(
                        "bitrouter-execution-revision",
                        ingress["bitrouter-execution-revision"].clone(),
                    );
                    StatusCode::BAD_REQUEST
                }
                "malformed-binding" => {
                    sent.insert("bitrouter-receipt-mode", "best-effort".parse()?);
                    StatusCode::BAD_REQUEST
                }
                "wrong-instance" => {
                    sent.insert("bitrouter-instance-id", "other-instance".parse()?);
                    StatusCode::FORBIDDEN
                }
                "header-bytes" => {
                    // Every individual value fits; duplicate values together exceed 64 KiB.
                    for _ in 0..4 {
                        sent.append("x-padding", "a".repeat(16 * 1024).parse()?);
                    }
                    StatusCode::BAD_REQUEST
                }
                "header-values" => {
                    for _ in 0..129 {
                        sent.append("x-padding", "a".parse()?);
                    }
                    StatusCode::BAD_REQUEST
                }
                _ => {
                    // Encoded JSON, not the input String length, is the bounded resource.
                    input.messages = vec![Message::text(
                        Role::User,
                        "\u{0000}".repeat(3 * 1024 * 1024),
                    )];
                    StatusCode::BAD_REQUEST
                }
            };
            let control = Arc::new(Control::default());
            let error = tokio::time::timeout(
                TIMEOUT,
                app.execute_native_controlled_with_headers(
                    input,
                    CallerContext::local(),
                    sent,
                    control.clone(),
                ),
            )
            .await?
            .err()
            .with_context(|| format!("{case} unexpectedly admitted"))?;
            ensure!(error.status() == expected.as_u16(), "{case}: {error}");
            ensure!(
                control.callbacks.load(Ordering::SeqCst) == 0,
                "{case} invoked control"
            );
            ensure!(
                fixture.service.admission.queued_for_test() == 0,
                "{case} entered admission"
            );
        }
        ensure!(provider.calls() == 0 && spies.transforms.load(Ordering::SeqCst) == 0);
        drop(leases);
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await
}

async fn change_authority(
    fixture: &Fixture,
    credential: &CredentialIssued,
    route: &ResourceRef,
    change: &str,
) -> Result<()> {
    let command = match change {
        "revoke" => Command::Revoke(
            fixture.tenant.clone(),
            credential.credential_id.clone(),
            Mutation {
                operation_id: "native-stop".try_into()?,
                expected_revision: credential.credential_revision.clone(),
                installation_id: fixture.installation.clone(),
            },
        ),
        "suspend" => Command::Tenant(
            fixture.tenant.clone(),
            TenantPut {
                operation_id: "native-stop".try_into()?,
                expected_revision: Some(
                    fixture
                        .service
                        .current()
                        .map_err(failure)?
                        .tenants
                        .get(&fixture.tenant)
                        .context("tenant")?
                        .revision
                        .clone(),
                ),
                state: TenantState::Suspended,
                installation_id: fixture.installation.clone(),
            },
        ),
        _ => Command::Binding(
            fixture.tenant.clone(),
            fixture.binding.clone(),
            BindingPut {
                operation_id: "native-stop".try_into()?,
                expected_revision: fixture.binding_result.resource_revision.clone(),
                principal_id: fixture.principal.clone(),
                role: TenantRole::TenantAdmin,
                scope: GrantScope::Resources {
                    resources: vec![route.key()],
                },
                expires_at: expiry()?,
                expected_management_owner: Some(
                    crate::tenancy::operator_principal("test-operator").map_err(failure)?,
                ),
                installation_id: fixture.installation.clone(),
            },
        ),
    };
    result(
        fixture
            .service
            .execute(fixture.operator.clone(), command)
            .await
            .map_err(failure)?,
    )?;
    Ok(())
}

#[tokio::test]
async fn native_plan_and_dispatch_waits_recheck_revocation_suspension_and_slot_scope() -> Result<()>
{
    for hold in [Hold::Plan, Hold::Dispatch] {
        for change in ["revoke", "suspend", "scope"] {
            let provider = Provider::start(false).await?;
            let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
            let gate = Arc::new(Gate::default());
            let release_on_drop = gate.release.clone().drop_guard();
            let checked = async {
                let (ingress, credential) = ready(&fixture, &route).await?;
                let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                    .with_tenancy(fixture.service.clone());
                let control = Arc::new(Control {
                    hold: Some((hold, gate.clone())),
                    ..Default::default()
                });
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
                ensure!(provider.calls() == 0);
                change_authority(&fixture, &credential, &route, change).await?;
                gate.release.cancel();
                let error = tokio::time::timeout(TIMEOUT, call)
                    .await??
                    .err()
                    .context("changed authority admitted native dispatch")?;
                let expected = if change == "revoke" {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::FORBIDDEN
                };
                ensure!(error.status() == expected.as_u16(), "{change}: {error}");
                ensure!(
                    provider.calls() == 0,
                    "{change} sent after the callback wait"
                );
                Ok(())
            }
            .await;
            drop(release_on_drop);
            finish(&fixture, provider, checked).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn cancelled_native_call_keeps_drain_and_lease_until_provider_and_outcome_cleanup()
-> Result<()> {
    for hold in [Hold::Dispatch, Hold::Outcome] {
        // Dispatch here means holding the real provider before response headers.
        let mut provider = Provider::start(hold == Hold::Dispatch).await?;
        let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
        let gate = Arc::new(Gate::default());
        let release_on_drop = gate.release.clone().drop_guard();
        let checked = async {
            let (ingress, _) = ready(&fixture, &route).await?;
            let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?.with_tenancy(fixture.service.clone());
            let control = Arc::new(Control {
                hold: (hold == Hold::Outcome).then(|| (Hold::Outcome, gate.clone())),
                ..Default::default()
            });
            let observed = control.clone();
            let call = AbortOnDropHandle::new(tokio::spawn(async move {
                app.execute_native_controlled_with_headers(prompt(), CallerContext::local(), ingress, control).await
            }));
            let request = provider.receive().await?;
            ensure!(request.body["model"] == "model-a");
            if hold == Hold::Outcome {
                tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
                ensure!(observed.attempts.lock().await.len() == 1);
            } else {
                ensure!(observed.attempts.lock().await.is_empty());
            }
            call.abort();
            ensure!(matches!(tokio::time::timeout(TIMEOUT, call).await?, Err(error) if error.is_cancelled()));
            let limits = Limits { active_executions: 1, queued_work: 0, learning_jobs: 0 };
            ensure!(matches!(fixture.service.admission.acquire(&fixture.tenant, &limits).now_or_never(), Some(Err(ErrorCode::CapacityExceeded))), "cancel released the active tenant lease");
            {
                let mut drain = Box::pin(fixture.service.pipelines.close_and_drain());
                ensure!(futures::poll!(&mut drain).is_pending(), "registry lost native cleanup");
            }
            let mut shutdown = Box::pin(fixture.service.shutdown());
            ensure!(futures::poll!(&mut shutdown).is_pending(), "shutdown skipped native cleanup");
            {
                let mut leases = Box::pin(fixture.service.admission.wait_drained());
                ensure!(futures::poll!(&mut leases).is_pending(), "shutdown released native custody");
            }
            provider.state.release.cancel();
            gate.release.cancel();
            tokio::time::timeout(TIMEOUT, shutdown).await??;
            tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
            let attempts = observed.attempts.lock().await;
            ensure!(attempts.len() == 1 && attempts[0].result.is_some() && attempts[0].error.is_none());
            let plans = observed.plans.lock().await;
            ensure!(attempts[0].request_id == plans.first().context("missing native plan")?.request_id);
            ensure!(provider.calls() == 1, "cancelled native work retried");
            Ok(())
        }.await;
        drop(release_on_drop);
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}

async fn revised_route(
    fixture: &mut Fixture,
    route: &ResourceRef,
    slot: &ResourceRef,
    url: &str,
    protocols: &[&str],
    count: bool,
) -> Result<ResourceRef> {
    fixture.service.shutdown().await?;
    let resources = Resources::with_key(fixture.installation.clone(), [29; 32])?;
    let mut route = route.clone();
    route.revision_id = "native-r2".try_into()?;
    resources.provision_bytes(&fixture.db, &serde_json::to_vec(&json!({
        "format":1, "installation_id":fixture.installation,
        "resources":[{"reference":route,"definition":{"kind":"route","selector":"bitrouter/owned","policy":null,
            "routing":{"providers":{"mock":{"api_base":url,"api_protocol":[{"*":protocols}],
                "models":[{"id":"model-a","input_token_counting":count.then_some("responses")}]}},
                "routers":{"owned":{"selection":{"kind":"model","model":"mock:model-a"},
                    "defaults":{"params":{"max_tokens":999,"max_completion_tokens":777,"max_output_tokens":888,"temperature":1.9}}}}},
            "credentials":[{"provider":"mock","account":"default","credential":slot}]}}]
    }))?).await?;
    let snapshot = store::load(&fixture.db, chrono::Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        OperationLimits::default(),
        snapshot,
        Arc::new(resources),
    )
    .await?;
    Ok(route)
}

#[tokio::test]
async fn configured_native_input_counter_stops_before_count_plan_or_generation() -> Result<()> {
    let provider = Provider::start(false).await?;
    let (mut fixture, route, slot) = provision_protocol(&provider.url, "responses").await?;
    let checked = async {
        let route = revised_route(
            &mut fixture,
            &route,
            &slot,
            &provider.url,
            &["responses"],
            true,
        )
        .await?;
        let (ingress, _) = ready(&fixture, &route).await?;
        let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone());
        for reject_plan in [false, true] {
            let control = Arc::new(Control {
                reject_plan,
                ..Default::default()
            });
            let error = tokio::time::timeout(
                TIMEOUT,
                app.execute_native_controlled_with_headers(
                    prompt(),
                    CallerContext::local(),
                    ingress.clone(),
                    control.clone(),
                ),
            )
            .await?
            .err()
            .context("required input counter was bypassed")?;
            ensure!(error.status() == StatusCode::FORBIDDEN.as_u16());
            ensure!(
                control.routing_commits.load(Ordering::SeqCst) == 1,
                "counter rejection occurred before native routing"
            );
            ensure!(control.counts.load(Ordering::SeqCst) == 0);
            ensure!(
                control.plans.lock().await.is_empty() && control.attempts.lock().await.is_empty()
            );
            ensure!(control.work.lock().await.is_empty());
        }
        ensure!(
            provider.calls() == 0,
            "required counter sent count or generation work"
        );
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await
}

#[tokio::test]
async fn tenant_reader_and_headerless_fences_preserve_legacy_app_behavior() -> Result<()> {
    let mut provider = Provider::start(false).await?;
    let (fixture, _, _) = provision_protocol(&provider.url, "chat_completions").await?;
    let checked = async {
        let spies = Arc::new(HostSpies::default());
        let tenant =
            host_app(&fixture, &provider.url, spies.clone())?.with_tenancy(fixture.service.clone());
        let legacy = host_app(&fixture, &provider.url, spies.clone())?;
        let caller = CallerContext::new("legacy-key", "legacy-principal");
        let ids = vec!["legacy-request".to_owned()];
        let costs =
            tokio::time::timeout(TIMEOUT, tenant.native_cost_observations(&caller, &ids)).await??;
        ensure!(costs.len() == 1 && costs[0].request_id == ids[0]);
        ensure!(costs[0].claims.is_empty() && costs[0].unknown_reason.is_some());
        ensure!(spies.reads.load(Ordering::SeqCst) == 0 && tenant.decision_model().is_none());
        let control = Arc::new(Control::default());
        ensure!(
            tokio::time::timeout(TIMEOUT, tenant.execute_native(prompt(), caller.clone()))
                .await?
                .is_err()
        );
        ensure!(
            tokio::time::timeout(
                TIMEOUT,
                tenant.execute_native_controlled(prompt(), caller.clone(), control.clone())
            )
            .await?
            .is_err()
        );
        ensure!(
            tokio::time::timeout(
                TIMEOUT,
                tenant.execute_native_stream(prompt(), caller.clone())
            )
            .await?
            .is_err()
        );
        ensure!(provider.calls() == 0 && control.callbacks.load(Ordering::SeqCst) == 0);
        ensure!(spies.transforms.load(Ordering::SeqCst) == 0);
        ensure!(legacy.decision_model().is_some());
        let costs =
            tokio::time::timeout(TIMEOUT, legacy.native_cost_observations(&caller, &ids)).await??;
        ensure!(
            costs
                .first()
                .context("legacy cost observation")?
                .unknown_reason
                .as_deref()
                == Some("legacy-source-was-read")
        );
        ensure!(spies.reads.load(Ordering::SeqCst) == 1);
        let response = tokio::time::timeout(
            TIMEOUT,
            legacy.execute_native_controlled(prompt(), caller, control.clone()),
        )
        .await??;
        ensure!(
            response
                .result
                .generation()
                .context("legacy generation")?
                .response_id
                .as_deref()
                == Some("chatcmpl-native")
        );
        let request = provider.receive().await?;
        ensure!(
            request
                .headers
                .get("authorization")
                .is_some_and(|value| value == "Bearer ambient-secret")
        );
        ensure!(spies.transforms.load(Ordering::SeqCst) == 1 && provider.calls() == 1);
        ensure!(
            control.plans.lock().await.len() == 1 && spies.decisions.load(Ordering::SeqCst) == 0
        );
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await
}

#[tokio::test]
async fn native_app_remains_production_fenced_with_valid_credential_and_binding() -> Result<()> {
    let provider = Provider::start(false).await?;
    let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
    let checked = async {
        let revision = draft(&fixture, &route, "native-production-draft", 25).await?;
        let credential = fixture.issue("native-production-execute").await?;
        let token = credential.secret.as_deref().context("secret")?;
        let caller = fixture.service.authenticate(token).map_err(failure)?;
        let capabilities = fixture.service.capabilities(&caller).map_err(failure)?;
        ensure!(
            !capabilities.revision_bound_dispatch
                && !capabilities.strict_receipts
                && !capabilities.dispatch_enabled
        );
        ensure!(
            capabilities
                .bind(revision.revision_id.clone(), "same-correlation".try_into()?)
                .is_err()
        );
        // A caller may fabricate the complete, current binding even while the
        // advertised capability is false. Never turn on the fixture readiness.
        let mut ingress = HeaderMap::new();
        for (name, value) in [
            ("bitrouter-tenancy-version", "1"),
            (
                "bitrouter-installation-id",
                capabilities.installation_id.as_str(),
            ),
            ("bitrouter-instance-id", capabilities.instance_id.as_str()),
            (
                "bitrouter-capability-revision",
                capabilities.capability_revision.as_str(),
            ),
            (
                "bitrouter-execution-revision",
                revision.revision_id.as_str(),
            ),
            ("bitrouter-correlation-id", "same-correlation"),
            ("bitrouter-receipt-mode", "strict"),
        ] {
            ingress.insert(name, value.parse()?);
        }
        ingress.insert("authorization", format!("Bearer {token}").parse()?);
        let spies = Arc::new(HostSpies::default());
        let app =
            host_app(&fixture, &provider.url, spies.clone())?.with_tenancy(fixture.service.clone());
        let control = Arc::new(Control::default());
        let error = tokio::time::timeout(
            TIMEOUT,
            app.execute_native_controlled_with_headers(
                prompt(),
                CallerContext::local(),
                ingress,
                control.clone(),
            ),
        )
        .await?
        .err()
        .context("production fence admitted native generation")?;
        ensure!(error.status() == StatusCode::FORBIDDEN.as_u16());
        ensure!(provider.calls() == 0 && control.callbacks.load(Ordering::SeqCst) == 0);
        ensure!(spies.transforms.load(Ordering::SeqCst) == 0);
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await
}
