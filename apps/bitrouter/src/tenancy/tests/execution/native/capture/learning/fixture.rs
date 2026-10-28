use super::*;
use crate::core::evolution::rubric;
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    identity::RevisionId,
};

#[derive(Clone)]
struct JudgeState {
    release: CancellationToken,
    observed: mpsc::Sender<Observed>,
    calls: Arc<AtomicUsize>,
    answer: String,
    usage: Value,
    hold_from: usize,
    status: StatusCode,
}
async fn serve_judge(
    State(state): State<JudgeState>,
    version: ::http::Version,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let ordinal = state.calls.fetch_add(1, Ordering::SeqCst) + 1;
    if state
        .observed
        .try_send(Observed {
            version,
            path: "/v1/chat/completions".into(),
            headers,
            body,
        })
        .is_err()
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if ordinal >= state.hold_from {
        state.release.cancelled().await;
    }
    if state.status != StatusCode::OK {
        return (state.status, Json(json!({"error":{"message":"deliberate bounded judge rejection","type":"invalid_request_error"}}))).into_response();
    }
    Json(json!({"id":"judge-reply", "object":"chat.completion", "created":1,"model":"judge-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":state.answer},"finish_reason":"stop"}],
        "usage":state.usage})).into_response()
}
pub(super) struct Judge {
    pub url: String,
    pub release: CancellationToken,
    pub calls: Arc<AtomicUsize>,
    observed: mpsc::Receiver<Observed>,
    task: Option<AbortOnDropHandle<std::io::Result<()>>>,
}
impl Judge {
    pub async fn new(hold: bool) -> Result<Self> {
        Self::hold_from(if hold { 1 } else { usize::MAX }).await
    }
    pub async fn incomplete_usage() -> Result<Self> {
        Self::build(usize::MAX, false).await
    }
    pub(super) async fn hold_from(hold_from: usize) -> Result<Self> {
        Self::build(hold_from, true).await
    }
    #[cfg(unix)]
    pub(super) async fn rejected() -> Result<Self> {
        Self::build_status(usize::MAX, true, StatusCode::BAD_REQUEST).await
    }
    async fn build(hold_from: usize, complete_usage: bool) -> Result<Self> {
        Self::build_status(hold_from, complete_usage, StatusCode::OK).await
    }
    async fn build_status(
        hold_from: usize,
        complete_usage: bool,
        status: StatusCode,
    ) -> Result<Self> {
        let items: Vec<_> = rubric::library().into_iter().map(|criterion| json!({
            "criterion_id":criterion.id,"applicability":"unknown","selection_reason":"finite fixture evidence",
            "score":{"status":"unknown"},"evidence":[],"explanation":"No unsupported positive claim"
        })).collect();
        let answer = serde_json::to_string(
            &json!({"rubric_version":rubric::RUBRIC_VERSION,"items":items,
            "diagnostics":[],"severe_violation":false,"violation_evidence":[],"summary":"Explicit fixture assessment"}),
        )?;
        let release = CancellationToken::new();
        let (send, observed) = mpsc::channel(16);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut usage = json!({"prompt_tokens":4,"completion_tokens":2,"total_tokens":6});
        if complete_usage {
            usage["prompt_tokens_details"] = json!({"cached_tokens":0,"cache_write_tokens":0});
            usage["completion_tokens_details"] = json!({"reasoning_tokens":0});
        }
        let state = JudgeState {
            release: release.clone(),
            observed: send,
            calls: calls.clone(),
            answer,
            usage,
            hold_from,
            status,
        };
        let router = Router::new()
            .route("/v1/chat/completions", post(serve_judge))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let task =
            AbortOnDropHandle::new(tokio::spawn(
                async move { axum::serve(listener, router).await },
            ));
        Ok(Self {
            url,
            release,
            calls,
            observed,
            task: Some(task),
        })
    }
    pub async fn receive(&mut self) -> Result<Observed> {
        tokio::time::timeout(TIMEOUT, self.observed.recv())
            .await?
            .context("judge did not receive request")
    }
    pub async fn stop(mut self) -> Result<()> {
        self.release.cancel();
        let task = self.task.take().context("judge already stopped")?;
        task.abort();
        match task.await {
            Err(error) if error.is_cancelled() => Ok(()),
            value => {
                value??;
                Ok(())
            }
        }
    }
}

pub(super) struct Ready {
    pub fixture: Fixture,
    pub subject: ResourceRef,
    pub judge: ResourceRef,
    pub revision: RevisionId,
    pub credential: CredentialIssued,
    profile: bool,
}
impl Ready {
    pub async fn new(
        subject_url: &str,
        judge_url: &str,
        profile: bool,
        mode: LearningMode,
        maximum: u32,
    ) -> Result<Self> {
        Self::from_fixture(
            Fixture::new().await?,
            None,
            subject_url,
            judge_url,
            profile,
            mode,
            maximum,
        )
        .await
    }
    pub async fn from_fixture(
        fixture: Fixture,
        owner: Option<crate::tenancy::ownership::Lease>,
        subject_url: &str,
        judge_url: &str,
        profile: bool,
        mode: LearningMode,
        maximum: u32,
    ) -> Result<Self> {
        Self::from_fixture_tracked(
            fixture,
            owner,
            (subject_url, judge_url),
            profile,
            mode,
            maximum,
            &mut |_| {},
        )
        .await
    }
    pub async fn from_fixture_tracked(
        mut fixture: Fixture,
        owner: Option<crate::tenancy::ownership::Lease>,
        endpoints: (&str, &str),
        profile: bool,
        mode: LearningMode,
        maximum: u32,
        started: &mut impl FnMut(&Service),
    ) -> Result<Self> {
        let (subject_url, judge_url) = endpoints;
        fixture.service.shutdown().await?;
        let resources = Resources::with_key(fixture.installation.clone(), [29; 32])?;
        let reference = |kind, name: &str| -> Result<ResourceRef> {
            Ok(ResourceRef {
                owner_kind: OwnerKind::Tenant,
                owner_id: fixture.tenant.as_str().try_into()?,
                resource_kind: kind,
                resource_id: name.try_into()?,
                revision_id: "r1".try_into()?,
            })
        };
        let subject = reference(ResourceKind::Route, "literal:subject")?;
        let judge = reference(ResourceKind::Route, "literal:judge")?;
        let subject_slot = reference(ResourceKind::Credential, "subject-slot")?;
        let judge_slot = reference(ResourceKind::Credential, "judge-slot")?;
        let route = |reference: &ResourceRef, slot: &ResourceRef, url: &str, model: &str| {
            json!({"reference":reference,"definition":{"kind":"route","selector":format!("mock:{model}"),"policy":null,
                "routing":{"providers":{"mock":{"api_base":url,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":model}]}}},
                "credentials":[{"provider":"mock","account":"default","credential":slot}]}})
        };
        let mut parent = route(&subject, &subject_slot, subject_url, "model-a");
        if profile {
            parent["definition"]["assessment"] = json!({"profile":"assessment_v1","route":judge});
        }
        resources.provision_bytes(&fixture.db, &serde_json::to_vec(&json!({"format":1,"installation_id":fixture.installation,
            "resources":[parent,route(&judge,&judge_slot,judge_url,"judge-model"),
                {"reference":subject_slot,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-upstream-secret"}}}},
                {"reference":judge_slot,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"explicit-judge-secret"}}}}]}))?).await?;
        let snapshot = store::load(&fixture.db, Utc::now()).await?;
        fixture.service = Service::start_prepared(
            fixture.db.clone(),
            owner,
            OperationLimits::default(),
            snapshot,
            Arc::new(resources),
        )
        .await?;
        started(&fixture.service);
        let credential = fixture
            .issue(&format!("assessment-client-{}", fixture.tenant))
            .await?;
        let mut ready = Self {
            fixture,
            subject,
            judge,
            revision: "initial".try_into()?,
            credential,
            profile,
        };
        ready.revision = ready
            .configuration(
                &format!("assessment-r1-{}", ready.fixture.tenant),
                mode,
                maximum,
            )
            .await?;
        ready
            .fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        Ok(ready)
    }
    pub async fn configuration(
        &self,
        operation: &str,
        mode: LearningMode,
        maximum: u32,
    ) -> Result<RevisionId> {
        self.configuration_with_active(operation, mode, maximum, 2)
            .await
    }
    pub async fn configuration_with_active(
        &self,
        operation: &str,
        mode: LearningMode,
        maximum: u32,
        active: u16,
    ) -> Result<RevisionId> {
        let mut input = self.fixture.configuration_input(operation)?;
        input.document.routes = vec![self.subject.clone()];
        if self.profile {
            input.document.routes.push(self.judge.clone());
        }
        input.document.learning.mode = mode;
        input.document.learning.max_evaluations = maximum;
        input.document.limits.active_executions = active;
        input.document.limits.queued_work = 0;
        input.document.limits.learning_jobs = 2;
        input.document.generation_defaults.max_output_tokens = 4096;
        match self
            .fixture
            .service
            .execute(
                self.fixture.operator.clone(),
                Command::Configuration(self.fixture.tenant.clone(), input),
            )
            .await
            .map_err(failure)?
        {
            crate::tenancy::store::StoredResponse::Configuration(value) => Ok(value.revision_id),
            _ => anyhow::bail!("configuration reply mismatch"),
        }
    }
    pub fn headers(&self) -> Result<HeaderMap> {
        headers(
            &self.fixture,
            self.credential
                .secret
                .as_deref()
                .context("credential secret")?,
            &self.revision,
        )
    }
    pub async fn port(&self) -> Result<Arc<Port>> {
        Ok(self.port_pair().await?.1)
    }
    pub async fn port_pair(&self) -> Result<(Arc<crate::tenancy::capture::Binding>, Arc<Port>)> {
        let (binding, port) = self.empty_port().await?;
        for event in sequence(self.subject.resource_id.as_str()) {
            port.record(event)
                .await
                .map_err(|error| anyhow::anyhow!("capture: {error}"))?;
        }
        Ok((binding, port))
    }
    pub async fn empty_port(&self) -> Result<(Arc<crate::tenancy::capture::Binding>, Arc<Port>)> {
        let prepared = self
            .fixture
            .service
            .prepare_capture(
                &self.headers()?,
                "assessment-fixture",
                "http://127.0.0.1:1",
                CancellationToken::new(),
            )
            .await
            .map_err(failure)?;
        let binding = prepared.binding;
        let port = Port::new(binding.clone());
        port.create().await?;
        Ok((binding, port))
    }
    pub async fn restart(&mut self) -> Result<()> {
        self.fixture.service.shutdown().await?;
        let resources = Resources::with_key(self.fixture.installation.clone(), [29; 32])?;
        let snapshot = store::load(&self.fixture.db, Utc::now()).await?;
        self.fixture.service = Service::start_prepared(
            self.fixture.db.clone(),
            None,
            OperationLimits::default(),
            snapshot,
            Arc::new(resources),
        )
        .await?;
        self.fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        Ok(())
    }
}
pub(super) async fn bodies(fixture: &Fixture, kind: &str) -> Result<Vec<Value>> {
    let rows = fixture
        .db
        .query_all(
            fixture.db.get_database_backend().build(
                &sea_orm::sea_query::Query::select()
                    .column(sea_orm::sea_query::Alias::new("body"))
                    .from(sea_orm::sea_query::Alias::new("checkpoint_evolution"))
                    .and_where(
                        sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new("kind"))
                            .eq(kind),
                    )
                    .order_by(
                        sea_orm::sea_query::Alias::new("record_id"),
                        sea_orm::sea_query::Order::Asc,
                    )
                    .limit(17)
                    .to_owned(),
            ),
        )
        .await?;
    ensure!(rows.len() <= 16, "fixture row budget");
    rows.into_iter()
        .map(|row| Ok(serde_json::from_str(&row.try_get::<String>("", "body")?)?))
        .collect()
}
pub(super) async fn drained(ready: &Ready) -> Result<()> {
    tokio::time::timeout(TIMEOUT, ready.fixture.service.admission.wait_drained())
        .await
        .context("learning owner failed to drain")?;
    ensure!(ready.fixture.service.current().is_ok());
    Ok(())
}

const NATIVE: &str = "RAW_NATIVE_SESSION_SECRET";
pub(super) fn native() -> &'static str {
    NATIVE
}
pub(super) fn sequence(model: &str) -> Vec<CaptureEvent> {
    let event = |kind, direction, call_id, method: &str, payload| CaptureEvent {
        kind,
        direction,
        call_id,
        method: method.into(),
        payload,
    };
    vec![
        event(
            Kind::Connected,
            Direction::Controller,
            None,
            "controller/connect",
            json!({}),
        ),
        event(
            Kind::Request,
            Direction::Client,
            Some(1),
            "session/new",
            json!({"cwd":"/fixture"}),
        ),
        event(
            Kind::Response,
            Direction::Agent,
            Some(1),
            "session/new",
            json!({"result":{"sessionId":NATIVE}}),
        ),
        event(
            Kind::Request,
            Direction::Client,
            Some(2),
            "session/set_config_option",
            json!({"sessionId":NATIVE,"configId":"model","value":model}),
        ),
        event(
            Kind::Response,
            Direction::Agent,
            Some(2),
            "session/set_config_option",
            json!({"result":{"configOptions":[{"id":"model","type":"select","currentValue":model,"options":[{"value":model,"name":"Route"}]}]}}),
        ),
        event(
            Kind::Request,
            Direction::Client,
            Some(3),
            "session/prompt",
            json!({"sessionId":NATIVE,"prompt":[{"type":"text","text":"explicit recorded task"}]}),
        ),
        event(
            Kind::Notification,
            Direction::Agent,
            None,
            "session/update",
            json!({"sessionId":NATIVE,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"recorded answer"}}}),
        ),
        event(
            Kind::Response,
            Direction::Agent,
            Some(3),
            "session/prompt",
            json!({"result":{"stopReason":"end_turn"}}),
        ),
    ]
}

impl Drop for Judge {
    fn drop(&mut self) {
        self.release.cancel();
    }
}
