//! Real leased storage, SDK transports, provider sends and v2 durable readback.
use super::*;
use crate::{
    config::tenancy::capacity::CapacityConfig, tenancy::tests::execution::draft_with_policy,
};
use bitrouter_ai::{
    control::ControlScope,
    protocol::{
        chat_completions::ChatRequest, messages::MessagesRequest, responses::ResponsesRequest,
    },
    stream::{ProtocolEnd, StreamItem},
    tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::ResourceRef,
        execution::{Outcome, ReceiptState},
        execution_v2::ExecutionReceiptV2,
        ordered::{GenerationProtocol, OrderedCandidateSelectionV1, OrderedDispatchBindingV2},
        validation::Validate,
    },
};
use bitrouter_sdk::{
    gateway::{GatewayClient, HttpVersion, TransportOptions, tenancy::Body},
    tenancy::{TenancyClient, TenancyOptions},
};
use futures::StreamExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU16};

const OPERATOR: &str = "ordered-test-operator-secret-at-least-32-bytes";

mod authorization;

mod archive;
mod barrier;
mod caller_timeout;
mod canonical;
mod defaults;
mod envelope;
mod generate_content_native;
mod native_controls;
mod predecessor;
mod private_adapter;
mod profiled;

#[derive(Clone)]
struct Provider {
    observed: Arc<Mutex<Vec<(String, HeaderMap)>>>,
    held_provider: Arc<Mutex<Option<Arc<barrier::Gate>>>>,
    malformed: Option<usize>,
    controls: Arc<ProviderControls>,
}

struct ProviderControls {
    rejection_status: AtomicU16,
    missing_usage: AtomicBool,
    held_stream: Mutex<Option<Arc<barrier::Gate>>>,
    bodies: Mutex<Vec<Value>>,
    native_state: Mutex<Option<String>>,
    stateful: AtomicBool,
}

impl Default for ProviderControls {
    fn default() -> Self {
        Self {
            rejection_status: AtomicU16::new(503),
            missing_usage: AtomicBool::new(false),
            held_stream: Mutex::new(None),
            bodies: Mutex::new(Vec::new()),
            native_state: Mutex::new(None),
            stateful: AtomicBool::new(false),
        }
    }
}
async fn ordered_provider(
    State(state): State<Provider>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let protocol = match uri.path() {
        "/v1/messages" => "messages",
        "/v1/responses" => "responses",
        _ => "chat_completions",
    };
    if let Ok(mut bodies) = state.controls.bodies.lock() {
        bodies.push(body.clone());
    } else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    {
        let Ok(mut observed) = state.observed.lock() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        observed.push((model.clone(), headers.clone()));
    }
    let authenticated = if protocol == "messages" {
        headers
            .get("x-api-key")
            .is_some_and(|value| value == "owned-upstream-secret")
            && !headers.contains_key("authorization")
    } else {
        headers
            .get("authorization")
            .is_some_and(|value| value == "Bearer owned-upstream-secret")
            && !headers.contains_key("x-api-key")
    };
    if !authenticated {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let gate = {
        let Ok(mut held) = state.held_provider.lock() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        held.take()
    };
    if let Some(gate) = gate {
        gate.entered.notify_one();
        match gate.release.acquire().await {
            Ok(permit) => permit.forget(),
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    }
    if state
        .malformed
        .is_some_and(|index| model == format!("model-{index}"))
    {
        return ([("content-type", "application/json")], "{broken").into_response();
    }
    if model != "model-0" {
        return (
            StatusCode::from_u16(state.controls.rejection_status.load(Ordering::SeqCst))
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({
                "error":{"message":"temporary"}, "usage":full_usage(protocol)
            })),
        )
            .into_response();
    }
    let usage = if state.controls.missing_usage.load(Ordering::SeqCst) {
        json!({"prompt_tokens":11,"completion_tokens":7})
    } else {
        full_usage(protocol)
    };
    if state.controls.stateful.load(Ordering::SeqCst) {
        return native_controls::provider_reply(&state.controls, protocol, &body, usage);
    }
    if body["stream"] == true {
        let gate = match state.controls.held_stream.lock() {
            Ok(mut gate) => gate.take(),
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        };
        return match canonical::provider_stream(protocol, usage) {
            Ok(body) => {
                if let Some(gate) = gate {
                    let Some((first, rest)) = body.split_once("\n\n") else {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    };
                    let first = format!("{first}\n\n");
                    let rest = rest.to_owned();
                    let stream = futures::stream::once(
                        async move { Ok::<_, std::io::Error>(first) },
                    )
                    .chain(futures::stream::once(async move {
                        gate.entered.notify_one();
                        gate.release
                            .acquire()
                            .await
                            .map_err(std::io::Error::other)?
                            .forget();
                        Ok(rest)
                    }));
                    (
                        [("content-type", "text/event-stream")],
                        axum::body::Body::from_stream(stream),
                    )
                        .into_response()
                } else {
                    ([("content-type", "text/event-stream")], body).into_response()
                }
            }
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }
    match json_response(protocol, usage) {
        Ok(body) => Json(body).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

struct OrderedHarness {
    fixture: Fixture,
    route: ResourceRef,
    revision: ConfigurationRevision,
    credential: CredentialIssued,
    endpoint: String,
    observed: Arc<Mutex<Vec<(String, HeaderMap)>>>,
    held_provider: Arc<Mutex<Option<Arc<barrier::Gate>>>>,
    controls: Arc<ProviderControls>,
    _provider: AbortOnDropHandle<std::io::Result<()>>,
    _gateway: AbortOnDropHandle<std::io::Result<()>>,
    _directory: tempfile::TempDir,
}
impl OrderedHarness {
    async fn new(count: usize, limit: u8, malformed: Option<usize>) -> Result<Self> {
        Self::with_defaults(count, limit, malformed, Value::Null).await
    }

    async fn with_defaults(
        count: usize,
        limit: u8,
        malformed: Option<usize>,
        defaults: Value,
    ) -> Result<Self> {
        Self::with_profiles(count, limit, malformed, defaults, None).await
    }

    async fn with_profiles(
        count: usize,
        limit: u8,
        malformed: Option<usize>,
        defaults: Value,
        profiles: Option<&[bitrouter_ai::tenancy::admission::AdmissionProfileV1]>,
    ) -> Result<Self> {
        Self::with_resource_edits(count, limit, malformed, defaults, profiles, |_| Ok(())).await
    }

    async fn with_resource_edits(
        count: usize,
        limit: u8,
        malformed: Option<usize>,
        defaults: Value,
        profiles: Option<&[bitrouter_ai::tenancy::admission::AdmissionProfileV1]>,
        edit: impl FnOnce(&mut Vec<Value>) -> Result<()>,
    ) -> Result<Self> {
        Self::with_native_mode(count, limit, malformed, defaults, profiles, false, edit).await
    }

    async fn with_native_mode(
        count: usize,
        limit: u8,
        malformed: Option<usize>,
        defaults: Value,
        profiles: Option<&[bitrouter_ai::tenancy::admission::AdmissionProfileV1]>,
        native_controls: bool,
        edit: impl FnOnce(&mut Vec<Value>) -> Result<()>,
    ) -> Result<Self> {
        Self::with_fixture_options(
            count,
            limit,
            malformed,
            defaults,
            profiles,
            (native_controls, None, CapacityConfig::default()),
            edit,
        )
        .await
    }

    async fn with_runtime_capacity(
        count: usize,
        limit: u8,
        malformed: Option<usize>,
        defaults: Value,
        profiles: Option<&[bitrouter_ai::tenancy::admission::AdmissionProfileV1]>,
        capacity: CapacityConfig,
        edit: impl FnOnce(&mut Vec<Value>) -> Result<()>,
    ) -> Result<Self> {
        Self::with_fixture_options(
            count,
            limit,
            malformed,
            defaults,
            profiles,
            (false, None, capacity),
            edit,
        )
        .await
    }

    async fn with_fixture_options(
        count: usize,
        limit: u8,
        malformed: Option<usize>,
        defaults: Value,
        profiles: Option<&[bitrouter_ai::tenancy::admission::AdmissionProfileV1]>,
        (native_controls, adapter, capacity): (
            bool,
            Option<Arc<private_adapter::FixtureAdapter>>,
            CapacityConfig,
        ),
        edit: impl FnOnce(&mut Vec<Value>) -> Result<()>,
    ) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let (db, owner, claim) = crate::db::connect_owned_tenancy(&format!(
            "sqlite://{}",
            directory.path().join("gateway.sqlite").display()
        ))
        .await?
        .claim()?;
        let mut fixture = Fixture::with_database(db, &mut |_| {}).await?;
        fixture.service.shutdown().await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}/v1", listener.local_addr()?);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let held_provider = Arc::new(Mutex::new(None));
        let controls = Arc::new(ProviderControls::default());
        let state = Provider {
            observed: observed.clone(),
            held_provider: held_provider.clone(),
            malformed,
            controls: controls.clone(),
        };
        let mut provider = Router::new()
            .route("/v1/chat/completions", post(ordered_provider))
            .route("/v1/messages", post(ordered_provider))
            .route("/v1/responses", post(ordered_provider))
            .with_state(state.clone());
        if let Some(adapter) = &adapter {
            provider = provider.merge(
                Router::new()
                    .route("/adapter/v1/chat/completions", post(private_adapter::serve))
                    .with_state((state, adapter.clone())),
            );
        }
        let provider = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, provider).await
        }));
        let resources = crate::tenancy::resources::Resources::with_key(
            fixture.installation.clone(),
            rand::random(),
        )?;
        let reference = |kind, name: &str| -> Result<ResourceRef> {
            Ok(ResourceRef {
                owner_kind: OwnerKind::Tenant,
                owner_id: fixture.tenant.as_str().try_into()?,
                resource_kind: kind,
                resource_id: name.try_into()?,
                revision_id: "r1".try_into()?,
            })
        };
        let route = reference(ResourceKind::Route, MODEL)?;
        let mut providers = serde_json::Map::new();
        let mut endpoints = Vec::new();
        let mut credentials = Vec::new();
        let mut definitions = Vec::new();
        for index in 0..count {
            let provider = format!("provider-{index}");
            let model = format!("model-{index}");
            let credential = reference(ResourceKind::Credential, &format!("static-slot-{index}"))?;
            let api_base = if adapter
                .as_ref()
                .is_some_and(|adapter| adapter.ordinal == index)
            {
                base.replace("/v1", "/adapter/v1")
            } else {
                base.clone()
            };
            providers.insert(provider.clone(), json!({"api_base":api_base, "api_protocol":[{"*":"chat_completions"}], "models":[{"id":model}]}));
            endpoints.push(json!({"provider":provider, "service_id":model}));
            definitions.push(json!({"reference":credential,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-upstream-secret"}}}}));
            credentials
                .push(json!({"provider":provider, "account":"default", "credential":credential}));
        }
        let mut routing = json!({"providers":providers,"models":{"ordered-root":{"strategy":"priority","endpoints":endpoints}}});
        let selector = if defaults.is_null() {
            "ordered-root"
        } else {
            routing["routers"] = json!({"ordered-defaults":{"selection":{"kind":"model","model":"ordered-root"},"defaults":{"params":defaults}}});
            "bitrouter/ordered-defaults"
        };
        definitions.push(
            json!({"reference":route,"definition":{"kind":"route","selector":selector,"policy":null,
            "routing":routing,"credentials":credentials}}),
        );
        if let Some(profiles) = profiles {
            ensure!(profiles.len() == count);
            let mut attachments = Vec::new();
            for (index, profile) in profiles.iter().enumerate() {
                let profile_ref = profiled::reference(index)?;
                for protocol in ["chat_completions", "messages", "responses"] {
                    attachments.push(json!({"inbound_protocol":protocol,"candidate_index":index,
                        "credential":credentials[index]["credential"],"profile":profile_ref}));
                }
                definitions.push(json!({"reference":profile_ref,"definition":{"kind":"admission_profile","profile":profile}}));
            }
            let route_definition = definitions
                .iter_mut()
                .find(|entry| entry["reference"] == json!(route))
                .context("route definition")?;
            route_definition["definition"]["kind"] = json!("profiled_route");
            route_definition["definition"]["attachments"] = json!(attachments);
        }
        edit(&mut definitions)?;
        // A retained route PolicyLock must match the configuration policy before
        // an ordered request can reach its capability refusal boundary.
        let policy = serde_json::from_value(
            definitions
                .iter()
                .find(|row| row["reference"]["resource_kind"] == "route")
                .context("route definition")?["definition"]["policy"]
                .clone(),
        )?;
        // Import prerequisites before the route without exceeding one import's
        // 256-resource limit. The tested route itself still retains all ordinals.
        definitions.sort_by_key(|entry| entry["reference"]["resource_kind"] == "route");
        let import_format = if definitions
            .iter()
            .any(|entry| entry["definition"]["kind"] == "admission_profile_v2")
        {
            2
        } else {
            1
        };
        for batch in definitions.chunks(128) {
            resources
                .provision_bytes(
                    &fixture.db,
                    &serde_json::to_vec(&json!({
                        "format":import_format, "installation_id":fixture.installation, "resources":batch
                    }))?,
                )
                .await?;
        }
        let config = crate::config::tenancy::TenancyConfig {
            capacity,
            ..Default::default()
        };
        fixture.service = crate::tenancy::Service::start_resources_with_continuations(
            fixture.db.clone(),
            Some(owner),
            crate::tenancy::store::load(&fixture.db, chrono::Utc::now()).await?,
            Arc::new(resources),
            crate::tenancy::Provisioning {
                import: None,
                continuations: &crate::config::ContinuationConfig::default(),
                claim: Some(claim),
                receipt_observer: crate::telemetry::receipt_latency::Sink::default(),
                config: &config,
            },
            None,
        )
        .await?;
        fixture.service.execution_settings.ordered_attempt_limit = limit;
        fixture.service.execution_settings.ordered_native_controls = native_controls;
        if adapter.is_some() {
            let endpoint = base
                .replace("/v1", "/adapter/v1/chat/completions")
                .parse()?;
            fixture.service.private_adapter = Some(Arc::new(
                crate::tenancy::execution::private_adapter::Peer::for_test(
                    endpoint,
                    private_adapter::KEY,
                )?,
            ));
        }
        let revision = if profiles.is_some() {
            profiled::draft_profiles(&fixture, &route).await?
        } else {
            draft_with_policy(&fixture, &route, "ordered-draft", 25, policy).await?
        };
        let credential = if profiles.is_some() {
            let references = definitions
                .iter()
                .map(|entry| serde_json::from_value(entry["reference"].clone()))
                .collect::<std::result::Result<Vec<ResourceRef>, _>>()?;
            profiled::grant_pool(&fixture, &references).await?
        } else {
            fixture.issue("ordered-client").await?
        };
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let auth = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig {
                credentials: vec![crate::config::ControlCredentialConfig {
                    id: "test-operator".into(),
                    token_env: "ORDERED_TEST_OPERATOR".into(),
                    scopes: vec![ControlScope::Tenancy],
                }],
                ..Default::default()
            },
            |_| Some(OPERATOR.into()),
        )?;
        let dispatch = Router::new()
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(axum::middleware::from_fn_with_state(
                fixture.service.clone(),
                crate::tenancy::http::dispatch,
            ));
        let app = crate::tenancy::http::router(fixture.service.clone(), auth).merge(dispatch);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let gateway =
            AbortOnDropHandle::new(tokio::spawn(
                async move { axum::serve(listener, app).await },
            ));
        Ok(Self {
            fixture,
            route,
            revision,
            credential,
            endpoint,
            observed,
            held_provider,
            controls,
            _provider: provider,
            _gateway: gateway,
            _directory: directory,
        })
    }
    fn token(&self) -> Result<&str> {
        self.credential.secret.as_deref().context("scoped token")
    }
    fn control(&self, operator: bool, version: HttpVersion) -> Result<TenancyClient> {
        Ok(TenancyClient::with_options(
            &self.endpoint,
            if operator { OPERATOR } else { self.token()? },
            TenancyOptions {
                http_version: version,
                request_timeout: Duration::from_secs(30),
                ..Default::default()
            },
        )?)
    }
    async fn binding(
        &self,
        indices: Vec<u16>,
        protocol: GenerationProtocol,
    ) -> Result<OrderedDispatchBindingV2> {
        let control = self.control(true, HttpVersion::Http1Only)?;
        let capabilities = control.ordered_capabilities().await?;
        ensure!(
            capabilities.dispatch_enabled
                && capabilities.attempt_limit
                    == self
                        .fixture
                        .service
                        .execution_settings
                        .ordered_attempt_limit
        );
        let manifest = control
            .candidate_manifest(
                &self.fixture.installation,
                &self.fixture.tenant,
                &self.revision.revision_id,
                &self.route,
                protocol,
            )
            .await?;
        ensure!(manifest.inbound_protocol == protocol && manifest.route == self.route);
        for (index, candidate) in manifest.candidates.iter().enumerate() {
            ensure!(usize::from(candidate.candidate_index) == index);
            ensure!(candidate.target.model_id == format!("model-{index}"));
        }
        Ok(capabilities.bind(
            self.revision.revision_id.clone(),
            "ordered-correlation".try_into()?,
            OrderedCandidateSelectionV1 {
                generation_defaults_digest: None,
                admission_mode: manifest.admission_mode,
                schema_version: 1,
                route: self.route.clone(),
                candidate_indices: indices,
            },
        )?)
    }
    async fn invoke(
        &self,
        binding: &OrderedDispatchBindingV2,
        protocol: GenerationProtocol,
        stream: bool,
        version: HttpVersion,
    ) -> Result<ExecutionId> {
        let client = GatewayClient::with_options(
            &self.endpoint,
            None,
            TransportOptions {
                http_version: version,
                request_timeout: Some(Duration::from_secs(30)),
                ..Default::default()
            },
        )?;
        let scoped = client.ordered_tenant(self.token()?, binding)?;
        macro_rules! call {
            ($method:ident, $request:ty, $body:expr, $terminal:expr) => {{
                let request: $request = serde_json::from_value($body)?;
                let reply = scoped.$method(&request).await?;
                ensure!(reply.metadata.status == 200);
                let id = reply.metadata.execution_id.context("execution identity")?;
                match reply.body {
                    Body::Json(value) => {
                        ensure!(!stream);
                        ensure!(serde_json::to_string(&value)?.contains("receipt reply"));
                    }
                    Body::Stream(mut events) => {
                        ensure!(stream);
                        let mut terminal = None;
                        while let Some(item) = events.next().await {
                            if let StreamItem::End(end) = item? {
                                ensure!(terminal.is_none());
                                terminal = Some(end);
                            }
                        }
                        ensure!(terminal == Some($terminal));
                    }
                }
                id
            }};
        }
        Ok(match protocol {
            GenerationProtocol::GenerateContent => {
                anyhow::bail!("native protocol requires the paired fixture")
            }
            GenerationProtocol::ChatCompletions => call!(
                chat_reply,
                ChatRequest,
                json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":stream}),
                ProtocolEnd::ChatDone
            ),
            GenerationProtocol::Messages => call!(
                messages_reply,
                MessagesRequest,
                json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"max_tokens":25,"stream":stream}),
                ProtocolEnd::MessagesStop
            ),
            GenerationProtocol::Responses => call!(
                responses_reply,
                ResponsesRequest,
                json!({"model":MODEL,"input":"hello","stream":stream}),
                ProtocolEnd::ResponsesCompleted
            ),
        })
    }
    async fn receipt(&self, id: &ExecutionId) -> Result<ExecutionReceiptV2> {
        let client = self.control(false, HttpVersion::Http1Only)?;
        let receipt = client
            .execution_v2(&self.fixture.installation, &self.fixture.tenant, id)
            .await?;
        receipt.validate()?;
        let evidence = client.usage_evidence_v2(&receipt).await?;
        evidence.validate_against(&receipt)?;
        ensure!(!evidence.truncated && evidence.attempts.len() == receipt.attempts.len());
        Ok(receipt)
    }
    fn observed(&self) -> Result<Vec<String>> {
        let observed = self
            .observed
            .lock()
            .map_err(|_| anyhow::anyhow!("provider observer poisoned"))?;
        ensure!(
            observed
                .iter()
                .all(|(_, headers)| !headers.contains_key("bitrouter-candidate-selection"))
        );
        Ok(observed.iter().map(|(model, _)| model.clone()).collect())
    }
}

#[tokio::test]
async fn ordered_sdk_executes_one_eight_nine_and_sixty_four_actual_hops_in_supplied_order()
-> Result<()> {
    for count in [1_u16, 8, 9, 64] {
        let harness = OrderedHarness::new(usize::from(count), 64, None).await?;
        let indices: Vec<_> = (0..count).rev().collect();
        let binding = harness
            .binding(indices.clone(), GenerationProtocol::ChatCompletions)
            .await?;
        let result = harness
            .invoke(
                &binding,
                GenerationProtocol::ChatCompletions,
                false,
                HttpVersion::Http1Only,
            )
            .await;
        let id = match result {
            Ok(id) => id,
            Err(error) => {
                let page = harness
                    .control(false, HttpVersion::Http1Only)?
                    .executions(
                        &harness.fixture.installation,
                        &harness.fixture.tenant,
                        &"ordered-correlation".try_into()?,
                        None,
                        100,
                    )
                    .await?;
                let mut receipts = Vec::new();
                for id in page.execution_ids {
                    receipts.push(harness.receipt(&id).await);
                }
                anyhow::bail!(
                    "{count} ordered candidates: {error}; sends: {:?}; receipts: {receipts:?}",
                    harness.observed()?
                );
            }
        };
        let receipt = harness.receipt(&id).await?;
        ensure!(
            receipt.outcome == Outcome::Completed && receipt.attempts.len() == usize::from(count)
        );
        ensure!(receipt.accepted_selection.candidate_indices == indices);
        ensure!(
            receipt
                .attempts
                .iter()
                .map(|attempt| attempt.candidate_index)
                .collect::<Vec<_>>()
                == indices
        );
        ensure!(
            harness.observed()?
                == indices
                    .iter()
                    .map(|index| format!("model-{index}"))
                    .collect::<Vec<_>>()
        );
        for (index, attempt) in receipt.attempts.iter().enumerate() {
            ensure!(
                attempt.observed_http_status
                    == Some(if index + 1 == usize::from(count) {
                        200
                    } else {
                        503
                    })
            );
        }
        let client = harness.control(false, HttpVersion::Http1Only)?;
        ensure!(
            client
                .execution(&harness.fixture.installation, &harness.fixture.tenant, &id)
                .await
                .is_err()
        );
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn ordered_sdk_three_protocols_json_and_streams_use_the_same_protected_receipt_on_h1_and_h2()
-> Result<()> {
    let harness = OrderedHarness::new(1, 8, None).await?;
    for protocol in [
        GenerationProtocol::ChatCompletions,
        GenerationProtocol::Messages,
        GenerationProtocol::Responses,
    ] {
        let binding = harness.binding(vec![0], protocol).await?;
        for version in [
            HttpVersion::Http1Only,
            #[cfg(feature = "http2")]
            HttpVersion::Http2PriorKnowledge,
        ] {
            for stream in [false, true] {
                let id = harness.invoke(&binding, protocol, stream, version).await?;
                let receipt = harness.receipt(&id).await?;
                ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
                ensure!(receipt.accepted_selection.inbound_protocol == protocol);
            }
        }
    }
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn ordered_unknown_body_stops_the_chain_and_retains_unknown() -> Result<()> {
    let harness = OrderedHarness::new(3, 64, Some(2)).await?;
    let binding = harness
        .binding(vec![2, 1, 0], GenerationProtocol::ChatCompletions)
        .await?;
    ensure!(
        harness
            .invoke(
                &binding,
                GenerationProtocol::ChatCompletions,
                false,
                HttpVersion::Http1Only
            )
            .await
            .is_err()
    );
    let page = harness
        .control(false, HttpVersion::Http1Only)?
        .executions(
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &"ordered-correlation".try_into()?,
            None,
            100,
        )
        .await?;
    ensure!(page.execution_ids.len() == 1);
    let receipt = harness.receipt(&page.execution_ids[0]).await?;
    ensure!(receipt.state == ReceiptState::Unknown && receipt.outcome == Outcome::Unknown);
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].candidate_index == 2);
    ensure!(harness.observed()? == vec!["model-2"]);
    harness.fixture.service.shutdown().await?;
    Ok(())
}
