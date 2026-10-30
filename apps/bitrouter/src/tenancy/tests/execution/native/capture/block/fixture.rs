use super::*;
use crate::gateway::routing::blocks::{
    allocation::BanditConfig,
    control::{BlockDefinition, BlockRule},
};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    identity::RevisionId,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[derive(Default)]
pub(super) struct Options<'a> {
    pub assessment: Option<(&'a str, AssessmentSource)>,
    pub pricing: Option<[Value; 2]>,
    pub usage: Option<Value>,
    pub bandit: Option<BanditConfig>,
}

pub(super) struct Host {
    pub fixture: Fixture,
    pub sibling: Option<(Fixture, HeaderMap, CredentialIssued)>,
    pub upstream: MockServer,
    pub directory: tempfile::TempDir,
    pub marker: PathBuf,
    pub endpoint: PathBuf,
    pub listener: Option<Listener>,
    pub session: Option<ControlledSession>,
    pub headers: Option<HeaderMap>,
    pub key: CredentialIssued,
    pub stop: CancellationToken,
    app: Option<Arc<App>>,
    upstream_url: String,
    pub database: PathBuf,
}
impl Host {
    pub async fn new() -> Result<Self> {
        Self::new_in(tempfile::tempdir()?, None).await
    }
    pub async fn new_in(
        directory: tempfile::TempDir,
        upstream_override: Option<&str>,
    ) -> Result<Self> {
        Self::new_options(
            directory,
            upstream_override,
            false,
            None,
            Options::default(),
        )
        .await
    }
    pub async fn pair() -> Result<Self> {
        Self::new_options(tempfile::tempdir()?, None, true, None, Options::default()).await
    }
    pub async fn non_hit() -> Result<Self> {
        Self::new_options(
            tempfile::tempdir()?,
            None,
            false,
            Some("never-this-actual-prompt".into()),
            Options::default(),
        )
        .await
    }
    pub async fn configured(options: Options<'_>) -> Result<Self> {
        Self::configured_in(tempfile::tempdir()?, options).await
    }
    pub async fn configured_in(directory: tempfile::TempDir, options: Options<'_>) -> Result<Self> {
        Self::new_options(directory, None, false, None, options).await
    }
    async fn new_options(
        directory: tempfile::TempDir,
        upstream_override: Option<&str>,
        two_tenants: bool,
        fingerprint: Option<String>,
        options: Options<'_>,
    ) -> Result<Self> {
        let db_path = directory.path().join("block.sqlite");
        let database =
            crate::db::connect_owned_tenancy(&format!("sqlite://{}", db_path.display())).await?;
        let mut fixture = Fixture::with_database(
            database.database().clone(),
            OperationLimits::default(),
            &mut |_| {},
        )
        .await?;
        let upstream = MockServer::start().await;
        let usage = options.usage.clone().unwrap_or_else(|| json!({"prompt_tokens":4,"completion_tokens":2,"total_tokens":6,
            "prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"completion_tokens_details":{"reasoning_tokens":0}}));
        Mock::given(method("POST")).and(path("/v1/chat/completions"))
            .respond_with(move |request: &wiremock::Request| {
                let Ok(body) = serde_json::from_slice::<Value>(&request.body) else { return ResponseTemplate::new(400); };
                ResponseTemplate::new(200).set_body_json(json!({"id":"block-result","object":"chat.completion","created":0,
                    "model":body["model"],"choices":[{"index":0,"message":{"role":"assistant","content":"native reply"},"finish_reason":"stop"}],
                    "usage":usage}))
            }).mount(&upstream).await;
        let upstream_url = upstream_override
            .map(str::to_owned)
            .unwrap_or_else(|| upstream.uri());
        let initialized = async {
            let mut sibling = if two_tenants {
                let tenant: bitrouter_ai::tenancy::identity::TenantId = "tenant-b".try_into()?;
                result(fixture.service.execute(fixture.operator.clone(), Command::Tenant(tenant.clone(), TenantPut {
                    operation_id: "create-block-tenant-b".try_into()?, expected_revision: None,
                    state: TenantState::Active, installation_id: fixture.installation.clone(),
                })).await.map_err(failure)?)?;
                let binding_result = result(fixture.service.execute(fixture.operator.clone(), Command::Binding(tenant.clone(), fixture.binding.clone(), BindingPut {
                    operation_id: "create-block-binding-b".try_into()?, expected_revision: None,
                    principal_id: fixture.principal.clone(), role: TenantRole::TenantAdmin,
                    scope: crate::tenancy::mutation::tenant_scope(), expires_at: expiry()?,
                    expected_management_owner: None, installation_id: fixture.installation.clone(),
                })).await.map_err(failure)?)?;
                Some(Fixture { service: fixture.service.clone(), db: fixture.db.clone(), operator: fixture.operator.clone(),
                    installation: fixture.installation.clone(), tenant, principal: fixture.principal.clone(),
                    binding: fixture.binding.clone(), binding_result })
            } else { None };
            fixture.service.shutdown().await?;
            let mut definition = BlockDefinition {
                block_id: "same-block".into(), source: "capture-fixture".into(), rationale: "Explicit fixture comparison".into(),
                rules: vec![BlockRule { selector: "mock:model-a".into(), fingerprint,
                    baseline_route: "mock:model-a".into(), challenger_route: "mock:model-b".into() }],
                independence_rationale: "One rule and one independently captured session".into(), dependencies: Default::default(),
                measurement_contract: crate::core::evolution::scoring::measurement_contract(
                    crate::acp_trajectory::checkpoint::types::AssessmentSource::Human,
                    &format!("owned-human/{}",fixture.principal), "owned-manual-rubric-v1")?,
                batch_sessions: 2,
                bandit: BanditConfig { maximum_challenger_sessions: 8, maximum_pending_challenger: 8,
                    initial_exposure_ppm: 500_000, maximum_exposure_ppm: 500_000, posterior_draws: 128,
                    ..Default::default() },
            };
            if let Some(bandit) = &options.bandit {
                definition.bandit = bandit.clone();
            }
            let resources = Resources::with_key(fixture.installation.clone(), [29;32])?;
            let mut imported = Vec::new();
            for subject in std::iter::once(&fixture).chain(sibling.iter()) {
                let route = resource(subject, ResourceKind::Route, "literal:subject")?;
                let slot = resource(subject, ResourceKind::Credential, "block-slot")?;
                let judge = resource(subject, ResourceKind::Route, "literal:judge")?;
                let mut definition = definition.clone();
                if options.assessment.is_some_and(|(_, source)| source == AssessmentSource::Agentic) {
                    definition.measurement_contract = crate::core::evolution::scoring::measurement_contract(
                        AssessmentSource::Agentic, &serde_json::to_string(&judge)?, crate::core::evolution::judge::JUDGE_VERSION)?;
                }
                let pricing = options.pricing.clone().unwrap_or_else(|| [
                    json!({"input_micro_usd_per_token":2.0,"output_micro_usd_per_token":3.0}),
                    json!({"input_micro_usd_per_token":2.0,"output_micro_usd_per_token":3.0}),
                ]);
                let mut parent = json!({"reference":route,"definition":{"kind":"route","selector":"mock:model-a","policy":null,
                    "block":{"profile":"block_v1","definition":definition},
                    "routing":{"providers":{"mock":{"api_base":format!("{}/v1",upstream_url),"api_protocol":[{"*":"chat_completions"}],
                        "models":[{"id":"model-a","pricing":pricing[0]},{"id":"model-b","pricing":pricing[1]}]}}},
                    "credentials":[{"provider":"mock","account":"default","credential":slot}]}});
                if let Some((url, _)) = options.assessment {
                    parent["definition"]["assessment"] = json!({"profile":"assessment_v1","route":judge});
                    let key = resource(subject, ResourceKind::Credential, "judge-slot")?;
                    imported.extend([
                        json!({"reference":judge,"definition":{"kind":"route","selector":"judge:judge-model","policy":null,
                            "routing":{"providers":{"judge":{"api_base":url,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"judge-model"}]}}},
                            "credentials":[{"provider":"judge","account":"default","credential":key}]}}),
                        json!({"reference":key,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"explicit-judge-secret"}}}}),
                    ]);
                }
                imported.extend([parent,
                    json!({"reference":slot,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-block-secret"}}}}),
                ]);
            }
            resources.provision_bytes(&fixture.db, &serde_json::to_vec(&json!({"format":1,
                "installation_id":fixture.installation,"resources":imported}))?).await?;
            let (db, owner, claim) = database.claim()?;
            let snapshot = store::load(&db, chrono::Utc::now()).await?;
            fixture.service = Service::start_resources_with_continuations(db, Some(owner), OperationLimits::default(), snapshot,
                Arc::new(resources), (None, &crate::config::ContinuationConfig::default(), Some(claim),
                    crate::telemetry::receipt_latency::Sink::default()), None).await?;
            fixture.service.dispatch_test_ready.store(true, Ordering::Release);
            let primary = configure(&fixture, options.assessment.is_some()).await?;
            let sibling = match sibling.take() {
                Some(mut other) => {
                    other.service = fixture.service.clone();
                    let (headers, key) = configure(&other, options.assessment.is_some()).await?;
                    Some((other, headers, key))
                }
                None => None,
            };
            Ok::<_, anyhow::Error>((primary.0, primary.1, sibling))
        }.await;
        let (headers, key, sibling) = match initialized {
            Ok(value) => value,
            Err(error) => {
                let cleanup = fixture.service.shutdown().await;
                cleanup?;
                return Err(error);
            }
        };
        Ok(Self {
            fixture,
            sibling,
            database: db_path,
            upstream_url,
            upstream,
            marker: directory.path().join("block-child.json"),
            endpoint: directory.path().join("endpoint.json"),
            directory,
            listener: None,
            session: None,
            headers: Some(headers),
            key,
            stop: CancellationToken::new(),
            app: None,
        })
    }
    pub fn current(&self) -> Result<&ControlledSession> {
        self.session.as_ref().context("actual Block controller")
    }
    pub async fn open(&mut self) -> Result<String> {
        self.open_options(false, None).await
    }
    pub async fn open_local_first(&mut self) -> Result<String> {
        self.open_options(true, None).await
    }
    pub async fn open_block_gate(&mut self, gate: &Path) -> Result<String> {
        self.open_options(false, Some(gate)).await
    }
    async fn open_options(&mut self, local_first: bool, gate: Option<&Path>) -> Result<String> {
        if self.listener.is_none() {
            let app = Arc::new(
                host_app(
                    &self.fixture,
                    &format!("{}/v1", self.upstream_url),
                    Arc::new(HostSpies::default()),
                )?
                .with_tenancy(self.fixture.service.clone()),
            );
            self.listener = Some(Listener::start(app.clone()).await?);
            self.app = Some(app);
        }
        let mut proposed = launch(
            &executable()?,
            &self.marker,
            &self.listener.as_ref().context("listener")?.origin,
            None,
        );
        let AcpTransport::Stdio { env, .. } = &mut proposed.agent.transport;
        env.insert(
            "CAPTURE_FIXTURE_ENDPOINT_MARKER".into(),
            self.endpoint.to_string_lossy().into_owned(),
        );
        if let Some(gate) = gate {
            env.insert(
                "CAPTURE_FIXTURE_BLOCK_GATE".into(),
                gate.to_string_lossy().into_owned(),
            );
        }
        if local_first {
            env.insert("CAPTURE_FIXTURE_LOCAL_FIRST_PROMPT".into(), "1".into());
        }
        self.stop = CancellationToken::new();
        self.session = Some(
            tokio::time::timeout(
                TIMEOUT,
                ControlledSession::open_authenticated(
                    self.app.as_ref().context("actual mounted App")?.clone(),
                    self.headers.as_ref().context("headers")?.clone(),
                    proposed,
                    self.stop.clone(),
                ),
            )
            .await??,
        );
        self.current()?
            .new_session(self.directory.path().to_owned())
            .await
    }
    pub async fn endpoint_headers(&self) -> Result<HeaderMap> {
        ensure!(tokio::fs::metadata(&self.endpoint).await?.len() <= 64 * 1024);
        let bytes = tokio::fs::read(&self.endpoint).await?;
        ensure!(bytes.len() <= 64 * 1024);
        let value: Value = serde_json::from_slice(&bytes)?;
        let mut headers = HeaderMap::new();
        for (name, value) in value["headers"]
            .as_object()
            .context("actual provider headers")?
        {
            headers.insert(
                http::header::HeaderName::from_bytes(name.as_bytes())?,
                value.as_str().context("header value")?.parse()?,
            );
        }
        ensure!(headers.contains_key(crate::tenancy::execution::BLOCK_LINK_HEADER));
        Ok(headers)
    }
    pub async fn close(&mut self) -> Result<()> {
        if let Some(session) = self.session.take() {
            tokio::time::timeout(TIMEOUT, session.shutdown()).await??;
        }
        reaped(&self.marker).await
    }
    pub async fn finish(mut self, result: Result<()>) -> Result<()> {
        self.stop.cancel();
        drop(self.session.take());
        let stopped =
            tokio::time::timeout(Duration::from_secs(40), self.fixture.service.shutdown())
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r);
        let child = if self.marker.exists() {
            reaped(&self.marker).await
        } else {
            Ok(())
        };
        let listener = match self.listener.take() {
            Some(listener) => listener.finish().await,
            None => Ok(()),
        };
        let closed = self
            .fixture
            .db
            .clone()
            .close()
            .await
            .map_err(anyhow::Error::from);
        let checked = result.and(stopped).and(child).and(listener).and(closed);
        if checked.is_err() {
            eprintln!("failed Block fixture: {}", self.directory.keep().display());
        }
        checked
    }
    pub async fn records(&self, kind: &str) -> Result<Vec<Value>> {
        records_for(&self.fixture, kind).await
    }
    pub async fn rejected(&self, headers: HeaderMap) -> Result<Value> {
        rejected(self.listener.as_ref().context("listener")?, headers).await
    }
}

pub(super) async fn records_for(fixture: &Fixture, kind: &str) -> Result<Vec<Value>> {
    let rows=fixture.db.query_all(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT record_id,scope_id,kind,session_key,revision,body FROM checkpoint_evolution WHERE installation_id=? AND tenant_id=? AND creator_principal_id=? AND kind=? LIMIT 129",
            [fixture.installation.as_str().into(),fixture.tenant.as_str().into(),fixture.principal.as_str().into(),kind.into()])).await?;
    ensure!(rows.len() <= 128);
    let mut values = Vec::new();
    for row in rows {
        let scope: String = row.try_get("", "scope_id")?;
        let id: String = row.try_get("", "record_id")?;
        let kind: String = row.try_get("", "kind")?;
        let session: Option<String> = row.try_get("", "session_key")?;
        let revision: i64 = row.try_get("", "revision")?;
        let body: String = row.try_get("", "body")?;
        ensure!(body.len() <= 2 * 1024 * 1024 && !body.contains("owned-block-secret"));
        let value = fixture.service.resources.open_block(
            &(&scope, &id, &kind, &session, revision),
            &serde_json::from_str(&body)?,
        )?;
        values.push(value);
    }
    Ok(values)
}
async fn rejected(listener: &Listener, headers: HeaderMap) -> Result<Value> {
    let reply=reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?.post(format!("{}/v1/chat/completions",listener.origin))
            .headers(headers).json(&json!({"model":"literal:subject","messages":[{"role":"user","content":"unowned attempt"}]})).send().await?;
    let status = reply.status();
    let bytes = reply.bytes().await?;
    ensure!(bytes.len() <= 8192);
    ensure!(
        [StatusCode::FORBIDDEN, StatusCode::CONFLICT].contains(&status),
        "unexpected rejection status {status}"
    );
    let error: bitrouter_ai::tenancy::operation::Error = serde_json::from_slice(&bytes)?;
    ensure!(matches!(
        error.code,
        ErrorCode::Forbidden | ErrorCode::CapabilityMismatch
    ));
    Ok(serde_json::to_value(error)?)
}

fn resource(fixture: &Fixture, kind: ResourceKind, name: &str) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: fixture.tenant.as_str().try_into()?,
        resource_kind: kind,
        resource_id: name.try_into()?,
        revision_id: "block-r1".try_into()?,
    })
}
async fn configure(fixture: &Fixture, paid: bool) -> Result<(HeaderMap, CredentialIssued)> {
    // Operations are keyed by installation and management principal, so the
    // second tenant needs a distinct command ID even when resource names match.
    let mut input =
        fixture.configuration_input(&format!("block-configuration-{}", fixture.tenant))?;
    input.document.routes = vec![resource(fixture, ResourceKind::Route, "literal:subject")?];
    input.document.learning.mode = LearningMode::Explore;
    if paid {
        input
            .document
            .routes
            .push(resource(fixture, ResourceKind::Route, "literal:judge")?);
        input.document.learning.max_evaluations = 1;
        input.document.limits.learning_jobs = 1;
    }
    input.document.limits.active_executions = 1;
    input.document.limits.queued_work = 0;
    let saved = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?;
    let revision: RevisionId = match saved {
        crate::tenancy::store::StoredResponse::Configuration(saved) => saved.revision_id,
        _ => anyhow::bail!("configuration reply"),
    };
    let key = fixture
        .issue(&format!("block-client-{}", fixture.tenant))
        .await?;
    Ok((
        headers(
            fixture,
            key.secret.as_deref().context("actual key")?,
            &revision,
        )?,
        key,
    ))
}
