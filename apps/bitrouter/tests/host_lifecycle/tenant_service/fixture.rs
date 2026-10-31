//! Public HTTP provisioning only; the startup file contains no resource catalog.
use super::*;
use base64::Engine;
use bitrouter_ai::tenancy::{
    authority::{
        BindingPut, CredentialCreate, CredentialIssued, GrantScope, OwnerKind, PrincipalPut,
        ResourceKey, ResourceKind, TenantPut, TenantRole, TenantState,
    },
    configuration::{Activation, ConfigurationPut, ConfigurationRevision, ResourceRef},
    identity::{InstallationId, PrincipalId, TenantId, Timestamp},
    resources::{CredentialBinding, Definition, ResourcePut},
};
use bitrouter_sdk::tenancy::TenancyOptions;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{header, method, path},
};

pub(super) const SECRET_A: &str = "sdk-process-alpha-provider-secret";
pub(super) const SECRET_B: &str = "sdk-process-beta-provider-secret";
pub(super) const SECRET_C: &str = "sdk-process-rotated-provider-secret";
const DEFAULT_SECRET: &str = "sdk-process-default-provider-secret";
pub(super) const REPLY: &str = "reply from dynamically provisioned resource";

fn reply_for(secret: &str) -> &'static str {
    match secret {
        SECRET_A => "alpha provider reply",
        SECRET_B => "beta provider reply",
        SECRET_C => "rotated provider reply",
        _ => "default provider reply",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Protocol {
    Chat,
    Messages,
    Responses,
    Decisions,
}

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Self::Chat => "chat_completions",
            Self::Messages => "messages",
            Self::Responses => "responses",
            Self::Decisions => "decisions",
        }
    }
    pub fn model(self) -> &'static str {
        match self {
            Self::Chat => "sdk:chat",
            Self::Messages => "sdk:messages",
            Self::Responses => "sdk:responses",
            Self::Decisions => "sdk:decisions",
        }
    }
    pub fn path(self) -> &'static str {
        match self {
            Self::Chat => "/chat/completions",
            Self::Messages => "/messages",
            Self::Responses => "/responses",
            Self::Decisions => "/decisions",
        }
    }
    pub fn end(self) -> Result<ProtocolEnd> {
        match self {
            Self::Chat => Ok(ProtocolEnd::ChatDone),
            Self::Messages => Ok(ProtocolEnd::MessagesStop),
            Self::Responses => Ok(ProtocolEnd::ResponsesCompleted),
            Self::Decisions => bail!("Decisions has no stream"),
        }
    }
    pub fn request(self, stream: bool) -> Value {
        match self {
            Self::Chat => {
                json!({"model":self.model(),"messages":[{"role":"user","content":"process SDK request"}],"stream":stream,"stream_options":{"include_usage":true}})
            }
            Self::Messages => {
                json!({"model":self.model(),"messages":[{"role":"user","content":"process SDK request"}],"max_tokens":64,"stream":stream})
            }
            Self::Responses => {
                json!({"model":self.model(),"input":"process SDK request","stream":stream})
            }
            Self::Decisions => {
                json!({"model":self.model(),"input":"process SDK request","questions":[{"type":"predicate","name":"q","instructions":"Evaluate evidence"}]})
            }
        }
    }
    pub fn usage(self) -> Value {
        match self {
            Self::Chat => {
                json!({"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,"prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":4}})
            }
            Self::Messages => {
                json!({"input_tokens":11,"output_tokens":7,"cache_read_input_tokens":3,"cache_creation_input_tokens":2})
            }
            _ => {
                json!({"input_tokens":11,"output_tokens":7,"total_tokens":18,"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":4}})
            }
        }
    }
    pub fn response(self) -> Value {
        match self {
            Self::Chat => {
                json!({"id":"chatcmpl-provider","object":"chat.completion","created":0,"model":"model","choices":[{"index":0,"message":{"role":"assistant","content":REPLY},"finish_reason":"stop"}],"usage":self.usage()})
            }
            Self::Messages => {
                json!({"id":"msg_provider","type":"message","role":"assistant","model":"model","content":[{"type":"text","text":REPLY}],"stop_reason":"end_turn","stop_sequence":null,"usage":self.usage()})
            }
            Self::Responses => {
                json!({"id":"resp_provider","object":"response","created_at":0,"status":"completed","model":"model","output":[{"id":"msg_provider","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":REPLY,"annotations":[]}]}],"usage":self.usage()})
            }
            Self::Decisions => {
                json!({"model":"model","answers":[{"type":"predicate","name":"q","probability":0.7}],"usage":self.usage()})
            }
        }
    }
    pub fn first_chat_chunk() -> String {
        format!(
            "data: {}\n\n",
            json!({"id":"chatcmpl-provider","object":"chat.completion.chunk","created":0,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":REPLY},"finish_reason":null}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6,"prompt_tokens_details":{"cached_tokens":1,"cache_write_tokens":0},"completion_tokens_details":{"reasoning_tokens":0}}})
        )
    }
    pub fn stream(self) -> Result<String> {
        // Native protocol framing, including explicit terminal events:
        // https://platform.openai.com/docs/api-reference/chat-streaming
        // https://platform.openai.com/docs/api-reference/responses-streaming
        // https://platform.claude.com/docs/en/api/streaming
        if self == Self::Chat {
            return Ok(format!(
                "{}data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                Self::first_chat_chunk(),
                json!({"id":"chatcmpl-provider","object":"chat.completion.chunk","created":0,"model":"model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":self.usage()}),
                json!({"id":"chatcmpl-provider","object":"chat.completion.chunk","created":0,"model":"model","choices":[],"usage":self.usage()})
            ));
        }
        let events = match self {
            Self::Messages => vec![
                json!({"type":"message_start","message":{"id":"msg_provider","type":"message","role":"assistant","model":"model","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":11,"output_tokens":0,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":REPLY}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":7}}),
                json!({"type":"message_stop"}),
            ],
            Self::Responses => vec![
                json!({"type":"response.created","sequence_number":0,"response":{"id":"resp_provider","object":"response","model":"model","status":"in_progress","output":[]}}),
                json!({"type":"response.output_text.delta","sequence_number":1,"item_id":"msg_provider","output_index":0,"content_index":0,"delta":REPLY}),
                json!({"type":"response.completed","sequence_number":2,"response":self.response()}),
            ],
            _ => bail!("unsupported named stream"),
        };
        events
            .iter()
            .map(|event| {
                Ok(format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().context("event kind")?
                ))
            })
            .collect::<Result<String>>()
    }
}

pub(super) fn start(upstream: &str) -> Result<(Host, String)> {
    let (mut host, inference, remote) = Host::prepare_profile(upstream, true)?;
    let config = host.home.join("private.yaml");
    let original = fs::read_to_string(&config)?;
    let (scaffold, _) = original
        .split_once("\nproviders:")
        .context("host startup scaffold")?;
    let source = scaffold.replace("tenancy: {enabled: true}", &format!(
        "tenancy:\n  enabled: true\n  wrapping_key_env: TEST_TENANCY_WRAPPING_KEY\n  resource_origins: [{upstream:?}]\n  execution_timeout_secs: 8\n  stream_idle_timeout_secs: 3"));
    ensure!(!source.contains("resource_import") && !source.contains("api_key"));
    fs::write(
        &config,
        format!("{source}\nproviders: {{}}\nmodels: {{}}\n"),
    )?;
    let key = base64::engine::general_purpose::STANDARD.encode([61; 32]);
    drop((inference, remote));
    host.spawn_with_wrapping_key(&config, Some(&key))?;
    Ok((host, key))
}

pub(super) async fn stop(host: &mut Host) -> Result<()> {
    ensure!(matches!(
        host.command(&DaemonCommand::Stop).await?,
        DaemonResponse::Ok
    ));
    ensure!(host.exited().await?.success());
    host.ensure_closed().await
}

pub(super) fn client(host: &Host, token: &str, version: HttpVersion) -> Result<TenancyClient> {
    Ok(TenancyClient::with_options(
        &format!("http://{}", host.remote),
        token,
        TenancyOptions {
            http_version: version,
            connect_timeout: Duration::from_secs(1),
            request_timeout: STEP_TIMEOUT,
            max_response_bytes: BODY_LIMIT,
            ..Default::default()
        },
    )?)
}

pub(super) fn gateway(host: &Host, token: &str, version: HttpVersion) -> Result<GatewayClient> {
    Ok(GatewayClient::with_options(
        &format!("http://{}", host.inference),
        Some(token),
        TransportOptions {
            http_version: version,
            connect_timeout: Duration::from_secs(1),
            read_timeout: Some(STEP_TIMEOUT),
            request_timeout: Some(STEP_TIMEOUT),
            max_response_bytes: BODY_LIMIT,
            max_event_bytes: BODY_LIMIT,
            ..Default::default()
        },
    )?)
}

pub(super) struct Owned {
    pub operator: TenancyClient,
    pub reader: TenancyClient,
    pub installation: InstallationId,
    pub tenant: TenantId,
    pub principal: PrincipalId,
    pub issued: CredentialIssued,
    pub slot: ResourceRef,
    pub routes: Vec<ResourceRef>,
    pub revision: ConfigurationRevision,
    pub reply: &'static str,
}

impl Owned {
    pub fn token(&self) -> Result<&str> {
        self.issued
            .secret
            .as_deref()
            .context("one-time ingress credential")
    }
    pub fn public(&self, value: &impl serde::Serialize) -> Result<()> {
        let bytes = serde_json::to_string(value)?;
        ensure!(bytes.len() <= BODY_LIMIT);
        clean(&bytes, &[self.token()?])
    }
    pub async fn binding(&self, correlation: &CorrelationId) -> Result<DispatchBinding> {
        let capabilities = self.reader.capabilities().await?;
        capabilities.validate()?;
        ensure!(
            capabilities.installation_id == self.installation
                && capabilities.dispatch_enabled
                && capabilities.strict_receipts
        );
        Ok(capabilities.bind(self.revision.revision_id.clone(), correlation.clone())?)
    }
    pub async fn configure(
        &mut self,
        operation: &str,
        authority: AuthorityRevision,
        previous: Option<RevisionId>,
    ) -> Result<()> {
        let input: ConfigurationPut = serde_json::from_value(json!({
            "operation_id":operation,"installation_id":self.installation,"expected_authority_revision":authority,
            "document":{"routes":self.routes,"policy":null,"generation_defaults":{"max_output_tokens":64,"temperature":0.0},"learning":{"mode":"off","max_evaluations":0},"limits":{"active_executions":2,"queued_work":0,"learning_jobs":0}}
        }))?;
        let revision = self.reader.put_configuration(&self.tenant, &input).await?;
        let result = self
            .reader
            .operation(&self.installation, &input.operation_id)
            .await?;
        let applied = self
            .reader
            .activate(
                &self.tenant,
                &Activation {
                    operation_id: format!("{operation}-activate").try_into()?,
                    installation_id: self.installation.clone(),
                    expected_active_revision: previous,
                    revision_id: revision.revision_id.clone(),
                    expected_authority_revision: applied(&result)?,
                },
            )
            .await?;
        super::applied(&applied)?;
        self.public(&result)?;
        self.public(&revision)?;
        ensure!(
            self.reader
                .configuration(&self.installation, &self.tenant, &revision.revision_id)
                .await?
                .document
                == input.document
        );
        ensure!(
            self.reader
                .active_revision(&self.installation, &self.tenant)
                .await?
                .revision_id
                .as_ref()
                == Some(&revision.revision_id)
        );
        self.revision = revision;
        Ok(())
    }
    pub async fn rotate(&mut self, upstream: &str) -> Result<ResourceRef> {
        let previous = self.slot.clone();
        let (slot, _) = put(
            &self.reader,
            &self.installation,
            &self.tenant,
            "rotate-slot",
            (ResourceKind::Credential, "upstream"),
            Some(previous.revision_id.clone()),
            Definition::Credential {
                secret: SECRET_C.into(),
            },
        )
        .await?;
        let route = self.routes.first().context("rotation route")?;
        let (route, authority) = put(
            &self.reader,
            &self.installation,
            &self.tenant,
            "rotate-route",
            (ResourceKind::Route, Protocol::Chat.model()),
            Some(route.revision_id.clone()),
            route_definition(upstream, Protocol::Chat, &slot),
        )
        .await?;
        self.slot = slot;
        self.reply = reply_for(SECRET_C);
        self.routes = vec![route];
        self.configure(
            "rotated-config",
            authority,
            Some(self.revision.revision_id.clone()),
        )
        .await?;
        Ok(previous)
    }
}

pub(super) fn clean(text: &str, additional: &[&str]) -> Result<()> {
    for secret in [SECRET_A, SECRET_B, SECRET_C, DEFAULT_SECRET, CONTROL_TOKEN]
        .into_iter()
        .chain(additional.iter().copied())
    {
        ensure!(
            !text.contains(secret),
            "private credential escaped into public output"
        );
    }
    Ok(())
}

async fn put(
    client: &TenancyClient,
    installation: &InstallationId,
    tenant: &TenantId,
    operation: &str,
    key: (ResourceKind, &str),
    expected: Option<RevisionId>,
    definition: Definition,
) -> Result<(ResourceRef, AuthorityRevision)> {
    let (kind, id) = key;
    let input = ResourcePut {
        format: 1,
        operation_id: operation.try_into()?,
        installation_id: installation.clone(),
        key: ResourceKey {
            owner_kind: OwnerKind::Tenant,
            owner_id: tenant.as_str().try_into()?,
            resource_kind: kind,
            resource_id: id.try_into()?,
        },
        expected_revision: expected,
        definition,
    };
    let result = client.put_resource(tenant, &input).await?;
    let authority = applied(&result)?;
    let reference = ResourceRef {
        owner_kind: input.key.owner_kind,
        owner_id: input.key.owner_id,
        resource_kind: kind,
        resource_id: input.key.resource_id,
        revision_id: result.resource_revision.context("resource revision")?,
    };
    let status = client
        .resource_status(installation, tenant, &reference)
        .await?;
    ensure!(!status.revoked && status.head_revision.as_ref() == Some(&reference.revision_id));
    clean(&serde_json::to_string(&status)?, &[])?;
    Ok((reference, authority))
}

fn route_definition(upstream: &str, protocol: Protocol, slot: &ResourceRef) -> Definition {
    Definition::Route {
        routing: json!({"providers":{"mock":{"api_base":upstream,"api_protocol":[{"*":protocol.name()}],"models":[{"id":"model"}]}}}),
        selector: "mock:model".into(),
        policy: None,
        credentials: vec![CredentialBinding {
            provider: "mock".into(),
            account: "default".into(),
            credential: slot.clone(),
        }],
    }
}

pub(super) async fn provision(
    host: &Host,
    upstream: &str,
    name: &str,
    secret: &str,
    protocols: &[Protocol],
    version: HttpVersion,
) -> Result<Owned> {
    let operator = client(host, CONTROL_TOKEN, version)?;
    let capabilities = operator.capabilities().await?;
    let installation = capabilities.installation_id;
    let resource_capabilities = operator.resource_capabilities(&installation).await?;
    ensure!(resource_capabilities.provisioning_enabled);
    let usage_capabilities = operator.usage_evidence_capabilities(&installation).await?;
    usage_capabilities.validate()?;
    let tenant: TenantId = name.try_into()?;
    let principal: PrincipalId = format!("{name}-principal").try_into()?;
    applied(
        &operator
            .put_tenant(
                &tenant,
                &TenantPut {
                    operation_id: format!("{name}-tenant").try_into()?,
                    expected_revision: None,
                    state: TenantState::Active,
                    installation_id: installation.clone(),
                },
            )
            .await?,
    )?;
    applied(
        &operator
            .put_principal(
                &principal,
                &PrincipalPut {
                    operation_id: format!("{name}-principal").try_into()?,
                    expected_revision: None,
                    issuer: "sdk-process".into(),
                    subject: name.into(),
                    installation_id: installation.clone(),
                },
            )
            .await?,
    )?;
    let expires_at: Timestamp = (chrono::Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        .try_into()?;
    let scope = GrantScope::Tenant {
        operator_pools: vec![],
    };
    let binding = format!("{name}-binding").try_into()?;
    let bound = operator
        .put_binding(
            &tenant,
            &binding,
            &BindingPut {
                operation_id: format!("{name}-binding").try_into()?,
                expected_revision: None,
                principal_id: principal.clone(),
                role: TenantRole::TenantAdmin,
                scope: scope.clone(),
                expires_at: expires_at.clone(),
                expected_management_owner: None,
                installation_id: installation.clone(),
            },
        )
        .await?;
    let issued = operator
        .issue_credential(
            &tenant,
            &CredentialCreate {
                operation_id: format!("{name}-issue").try_into()?,
                expected_authority_revision: applied(&bound)?,
                principal_id: principal.clone(),
                permissions: TenantRole::TenantAdmin.permissions().to_vec(),
                scope,
                expires_at,
                rotate_credential_id: None,
                binding_id: binding,
                installation_id: installation.clone(),
            },
        )
        .await?;
    ensure!(issued.tenant_id == tenant && issued.principal_id == principal);
    let reader = client(
        host,
        issued.secret.as_deref().context("issued credential")?,
        version,
    )?;
    let (slot, mut authority) = put(
        &reader,
        &installation,
        &tenant,
        "create-slot",
        (ResourceKind::Credential, "upstream"),
        None,
        Definition::Credential {
            secret: secret.into(),
        },
    )
    .await?;
    let mut routes = Vec::new();
    for protocol in protocols {
        let (route, revision) = put(
            &reader,
            &installation,
            &tenant,
            &format!("create-{}", protocol.name()),
            (ResourceKind::Route, protocol.model()),
            None,
            route_definition(upstream, *protocol, &slot),
        )
        .await?;
        routes.push(route);
        authority = revision;
    }
    let input: ConfigurationPut = serde_json::from_value(
        json!({"operation_id":"initial-config","installation_id":installation,"expected_authority_revision":authority,
        "document":{"routes":routes,"policy":null,"generation_defaults":{"max_output_tokens":64,"temperature":0.0},"learning":{"mode":"off","max_evaluations":0},"limits":{"active_executions":2,"queued_work":0,"learning_jobs":0}}}),
    )?;
    let revision = reader.put_configuration(&tenant, &input).await?;
    let result = reader.operation(&installation, &input.operation_id).await?;
    applied(
        &reader
            .activate(
                &tenant,
                &Activation {
                    operation_id: "initial-activate".try_into()?,
                    installation_id: installation.clone(),
                    expected_active_revision: None,
                    revision_id: revision.revision_id.clone(),
                    expected_authority_revision: applied(&result)?,
                },
            )
            .await?,
    )?;
    let owned = Owned {
        operator,
        reader,
        installation,
        tenant,
        principal,
        issued,
        slot,
        routes,
        revision,
        reply: reply_for(secret),
    };
    owned.public(&owned.revision)?;
    Ok(owned)
}

pub(super) async fn mount(
    provider: &MockServer,
    protocol: Protocol,
    secret: &str,
    stream: bool,
) -> Result<()> {
    let template = if stream {
        ResponseTemplate::new(200).set_body_raw(protocol.stream()?, "text/event-stream")
    } else {
        let mut response = protocol.response();
        if protocol == Protocol::Chat {
            response["choices"][0]["message"]["content"] = json!(reply_for(secret));
        }
        ResponseTemplate::new(200).set_body_json(response)
    };
    let (name, value) = if protocol == Protocol::Messages {
        ("x-api-key", secret.to_owned())
    } else {
        ("authorization", format!("Bearer {secret}"))
    };
    Mock::given(method("POST"))
        .and(path(protocol.path()))
        .and(header(name, value))
        .and(move |request: &wiremock::Request| {
            serde_json::from_slice::<Value>(&request.body).is_ok_and(|v| {
                v["model"] == "model" && (v["stream"].as_bool() == Some(true)) == stream
            })
        })
        .respond_with(template)
        .mount(provider)
        .await;
    Ok(())
}

pub(super) async fn calls(
    provider: &MockServer,
    expected: &[(Protocol, &str, bool)],
) -> Result<()> {
    let requests = provider
        .received_requests()
        .await
        .context("provider request recording")?;
    ensure!(
        requests.len() == expected.len(),
        "unexpected provider send count: {} instead of {}",
        requests.len(),
        expected.len()
    );
    let mut remaining = expected.to_vec();
    for request in &requests {
        ensure!(request.body.len() < BODY_LIMIT);
        let body: Value = serde_json::from_slice(&request.body)?;
        let index = remaining
            .iter()
            .position(|(protocol, secret, stream)| {
                let (name, value) = if *protocol == Protocol::Messages {
                    ("x-api-key", secret.to_string())
                } else {
                    ("authorization", format!("Bearer {secret}"))
                };
                request.url.path() == protocol.path()
                    && (body["stream"].as_bool() == Some(true)) == *stream
                    && request
                        .headers
                        .get(name)
                        .is_some_and(|header| header == value.as_str())
            })
            .context("unexpected provider identity or path")?;
        let (protocol, secret, stream) = remaining.remove(index);
        ensure!(
            request.method == "POST"
                && request.url.path() == protocol.path()
                && request.body.len() < BODY_LIMIT
        );
        let name = if protocol == Protocol::Messages {
            "x-api-key"
        } else {
            "authorization"
        };
        let auth = request
            .headers
            .get(name)
            .context("provider credential header")?
            .to_str()?;
        ensure!(
            auth == if protocol == Protocol::Messages {
                secret.to_string()
            } else {
                format!("Bearer {secret}")
            }
        );
        ensure!(body["model"] == "model" && (body["stream"].as_bool() == Some(true)) == stream);
        clean(std::str::from_utf8(&request.body)?, &[])?;
        ensure!(
            !request
                .headers
                .keys()
                .any(|name| name.as_str().starts_with("bitrouter-"))
        );
    }
    Ok(())
}

/// A real provider socket that withholds both responses until both callers arrive.
/// The wiremock backend still records and validates the native provider requests.
pub(super) struct ProviderGate {
    pub endpoint: String,
    state: Arc<GateState>,
    arrivals: mpsc::Receiver<(String, Value)>,
    stop: CancellationToken,
    task: JoinHandle<std::io::Result<()>>,
}

struct GateState {
    upstream: String,
    client: reqwest::Client,
    waiting: AtomicUsize,
    release: tokio::sync::Semaphore,
    arrivals: mpsc::Sender<(String, Value)>,
}

async fn gated_response(
    axum::extract::State(state): axum::extract::State<Arc<GateState>>,
    request: axum::extract::Request,
) -> std::result::Result<axum::response::Response, axum::http::StatusCode> {
    use axum::{http::StatusCode, response::IntoResponse};
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, BODY_LIMIT)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    // Retain the saturating reservation on the MSRV and Rust 1.99, which
    // deprecated fetch_update in favor of the newer try_update spelling.
    let mut waiting = state.waiting.load(Ordering::SeqCst);
    let reserved = loop {
        let Some(next) = waiting.checked_sub(1) else {
            break false;
        };
        match state
            .waiting
            .compare_exchange_weak(waiting, next, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => break true,
            Err(current) => waiting = current,
        }
    };
    if reserved {
        let authorization = parts
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let value = serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
        state
            .arrivals
            .try_send((authorization.into(), value))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        tokio::time::timeout(STEP_TIMEOUT, state.release.acquire())
            .await
            .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .forget();
    }
    let mut headers = parts.headers;
    headers.remove("host");
    let response = state
        .client
        .request(
            parts.method,
            format!("{}{}", state.upstream, parts.uri.path()),
        )
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if body.len() > BODY_LIMIT {
        return Err(StatusCode::BAD_GATEWAY);
    }
    Ok((status, headers, body).into_response())
}

impl ProviderGate {
    pub async fn start(upstream: &str) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let (sender, arrivals) = mpsc::channel(2);
        let state = Arc::new(GateState {
            upstream: upstream.into(),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(STEP_TIMEOUT)
                .build()?,
            waiting: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
            arrivals: sender,
        });
        let router = axum::Router::new()
            .fallback(gated_response)
            .with_state(state.clone());
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });
        Ok(Self {
            endpoint,
            state,
            arrivals,
            stop,
            task,
        })
    }

    pub fn arm(&self) -> Result<()> {
        ensure!(self.state.release.available_permits() == 0);
        ensure!(self.state.waiting.swap(2, Ordering::SeqCst) == 0);
        Ok(())
    }

    pub async fn release_pair(&mut self) -> Result<()> {
        let mut identities = Vec::new();
        for _ in 0..2 {
            let (authorization, body) = tokio::time::timeout(STEP_TIMEOUT, self.arrivals.recv())
                .await
                .context("both tenant requests must reach provider before either response")?
                .context("provider gate closed")?;
            ensure!(body["model"] == "model" && body["stream"].as_bool() != Some(true));
            clean(&serde_json::to_string(&body)?, &[])?;
            identities.push(authorization);
        }
        identities.sort();
        ensure!(identities == vec![format!("Bearer {SECRET_A}"), format!("Bearer {SECRET_B}")]);
        self.state.release.add_permits(2);
        Ok(())
    }

    pub async fn close(&mut self) -> Result<()> {
        self.state.release.close();
        self.stop.cancel();
        tokio::time::timeout(CLEANUP_TIMEOUT, &mut self.task)
            .await
            .context("provider gate cleanup timeout")??
            .context("provider gate failed")
    }
}

impl Drop for ProviderGate {
    fn drop(&mut self) {
        self.state.release.close();
        self.stop.cancel();
        if !self.task.is_finished() {
            self.task.abort();
        }
    }
}

pub(super) async fn default_owner(
    host: &Host,
    upstream: &str,
    version: HttpVersion,
) -> Result<Owned> {
    provision(
        host,
        upstream,
        "default-tenant",
        DEFAULT_SECRET,
        &[Protocol::Chat],
        version,
    )
    .await
}

pub(super) struct HeldStream {
    pub endpoint: String,
    pub calls: Arc<AtomicUsize>,
    pub observed: mpsc::Receiver<(bool, Value)>,
    stop: CancellationToken,
    task: JoinHandle<std::io::Result<()>>,
}

impl HeldStream {
    pub async fn start() -> Result<Self> {
        use axum::response::IntoResponse;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let (sender, observed) = mpsc::channel(4);
        let calls = Arc::new(AtomicUsize::new(0));
        let state = (sender, calls.clone());
        let router = axum::Router::new()
            .route(
                "/chat/completions",
                axum::routing::post(
                    |axum::extract::State((sender, calls)): axum::extract::State<(
                        mpsc::Sender<(bool, Value)>,
                        Arc<AtomicUsize>,
                    )>,
                     headers: axum::http::HeaderMap,
                     axum::Json(body): axum::Json<Value>| async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let authorized = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .is_some_and(|v| v == format!("Bearer {SECRET_A}"));
                        if sender.try_send((authorized, body)).is_err() {
                            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
                        }
                        let stream = futures::stream::once(async {
                            Ok::<_, std::io::Error>(Protocol::first_chat_chunk())
                        })
                        .chain(futures::stream::pending());
                        (
                            [("content-type", "text/event-stream")],
                            axum::body::Body::from_stream(stream),
                        )
                            .into_response()
                    },
                ),
            )
            .with_state(state);
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });
        Ok(Self {
            endpoint,
            calls,
            observed,
            stop,
            task,
        })
    }
    pub async fn close(&mut self) -> Result<()> {
        self.stop.cancel();
        tokio::time::timeout(CLEANUP_TIMEOUT, &mut self.task)
            .await
            .context("held provider cleanup timeout")??
            .context("held provider failed")
    }
}
impl Drop for HeldStream {
    fn drop(&mut self) {
        self.stop.cancel();
        if !self.task.is_finished() {
            self.task.abort();
        }
    }
}
