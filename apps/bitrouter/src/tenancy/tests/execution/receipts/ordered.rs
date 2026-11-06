//! Real leased storage, SDK transports, provider sends and v2 durable readback.
use super::*;
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

const OPERATOR: &str = "ordered-test-operator-secret-at-least-32-bytes";

mod authorization;

mod archive;
mod barrier;
mod caller_timeout;
mod defaults;
mod predecessor;

#[derive(Clone)]
struct Provider {
    observed: Arc<Mutex<Vec<(String, HeaderMap)>>>,
    held_provider: Arc<Mutex<Option<Arc<barrier::Gate>>>>,
    malformed: Option<usize>,
}
async fn ordered_provider(
    State(state): State<Provider>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    {
        let Ok(mut observed) = state.observed.lock() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        observed.push((model.clone(), headers.clone()));
    }
    if headers
        .get("authorization")
        .is_none_or(|value| value != "Bearer owned-upstream-secret")
    {
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
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error":{"message":"temporary"}, "usage":full_usage("chat_completions")
            })),
        )
            .into_response();
    }
    if body["stream"] == true {
        return match super::wire::chat_stream(full_usage("chat_completions"), true) {
            Ok(body) => ([("content-type", "text/event-stream")], body).into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }
    match json_response("chat_completions", full_usage("chat_completions")) {
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
        let provider = Router::new()
            .route("/v1/chat/completions", post(ordered_provider))
            .with_state(Provider {
                observed: observed.clone(),
                held_provider: held_provider.clone(),
                malformed,
            });
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
            providers.insert(provider.clone(), json!({"api_base":base, "api_protocol":[{"*":"chat_completions"}], "models":[{"id":model}]}));
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
        resources
            .provision_bytes(
                &fixture.db,
                &serde_json::to_vec(&json!({
                    "format":1, "installation_id":fixture.installation, "resources":definitions
                }))?,
            )
            .await?;
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
                config: &crate::config::tenancy::TenancyConfig::default(),
            },
            None,
        )
        .await?;
        fixture.service.execution_settings.ordered_attempt_limit = limit;
        let revision = draft(&fixture, &route, "ordered-draft", 25).await?;
        let credential = fixture.issue("ordered-client").await?;
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
