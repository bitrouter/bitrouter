use super::*;
use crate::{
    gateway::routing::{
        experiment_identity::{
            HISTORY_OPTIMIZER_ID, HISTORY_OPTIMIZER_VERSION, experiment_id,
            explore_compiler_config_digest, treatment_context_digest,
        },
        exploration::{OptimizationGate, PolicyOptimizationState, RouteExploration},
        workflow::predictive::compiled_predictor_contract,
    },
    policy_lock::{
        CertificateSource, CompilerIdentity, PolicyArtifact, PolicyCertificate, PolicyDefinition,
        PolicyLock, PromotionVerdict, RouteOwner, validate_document,
    },
};
use axum::{body::Body, http::Request};
use bitrouter_ai::routing::PolicyModelTarget;
use tower::ServiceExt;

pub(super) fn signed_experiment(name: &str, ppm: u32, cap: u32) -> Result<PolicyLock> {
    const PARENT: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const EVIDENCE: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    // This fixture has no semantic assessor. The serving projection abstains;
    // observed prompt heuristics cannot stand in for a paid assessment.
    const KEY: &str = "semantic_route/v1|unknown|unknown|guarded";
    let gate = OptimizationGate {
        objective: Default::default(),
        minimum_tasks_per_arm: 1,
        maximum_challenger_tasks: cap,
        minimum_pass_rate_ppm: 900_000,
        evaluator_config_digest: None,
    };
    let context = treatment_context_digest(name, KEY, "economy", "challenger", ppm, &gate)?;
    let compiler_digest = explore_compiler_config_digest(name, "challenger", ppm, &gate)?;
    let active = RouteExploration {
        experiment_id: experiment_id(PARENT, &context)?,
        target_request_key: KEY.into(),
        champion_tier: "economy".into(),
        challenger_tier: "challenger".into(),
        challenger_exposure_ppm: ppm,
        gate,
    };
    let policy = PolicyDefinition {
        tiers: [
            (
                "economy".into(),
                PolicyModelTarget::model_only("mock:model-a".into()),
            ),
            (
                "challenger".into(),
                PolicyModelTarget::model_only("mock:model-b".into()),
            ),
        ]
        .into(),
        default_tier: Some("economy".into()),
        predictor: Some(compiled_predictor_contract()),
        progress_guard: Some(crate::trajectory::guard::ProgressGuardPolicy {
            escalation_tier: "economy".into(),
            protected_tiers: ["economy".into(), "challenger".into()].into(),
            max_consecutive_unprotected: None,
            max_same_projection_unprotected: None,
            max_recovery_count: None,
            max_episode_requests: Some(128),
            max_episode_elapsed_ms: None,
            max_episode_cost_micro_usd: None,
            hold_for_requests: 1,
            incomplete_history: crate::trajectory::guard::IncompleteHistoryAction::Observe,
        }),
        optimization: Some(PolicyOptimizationState {
            active: Some(active),
            rejections: Vec::new(),
        }),
        ..Default::default()
    };
    let certificate = PolicyCertificate {
        classifier_digest: None,
        owner: RouteOwner::Compiler,
        selected_tier: "economy".into(),
        baseline_tier: Some("economy".into()),
        source: CertificateSource::Mixed,
        eligible_episodes: 1,
        independent_tasks: 1,
        quality: None,
        economics: None,
        latency: None,
        critical_violations: 0,
        verdict: PromotionVerdict::Experiment,
        evaluator_config_digest: None,
        compiler_config_digest: compiler_digest.clone(),
        evidence_digest: EVIDENCE.into(),
    };
    let document = PolicyLock {
        artifact: Some(PolicyArtifact {
            parent_digest: Some(PARENT.into()),
            evidence_root: EVIDENCE.into(),
            eval_snapshot_root: Some(EVIDENCE.into()),
            source_snapshot_time_unix_ms: 1,
            compiler: CompilerIdentity {
                id: HISTORY_OPTIMIZER_ID.into(),
                version: HISTORY_OPTIMIZER_VERSION,
                config_digest: compiler_digest,
            },
        }),
        policies: [(name.into(), policy)].into(),
        certificates: [(name.into(), [(KEY.into(), certificate)].into())].into(),
        ..Default::default()
    };
    validate_document(&document).context("active experiment fixture must be valid")?;
    // A valid-looking digest with the wrong parent/treatment identity must fail;
    // otherwise a fixture could accidentally bypass signed experiment validation.
    let mut corrupted = document.clone();
    corrupted
        .policies
        .get_mut(name)
        .and_then(|policy| policy.optimization.as_mut())
        .and_then(|optimization| optimization.active.as_mut())
        .context("fixture active experiment")?
        .experiment_id = PARENT.into();
    ensure!(
        validate_document(&corrupted).is_err(),
        "fixture must bind the experiment identity"
    );
    Ok(document)
}

pub(super) async fn upstream() -> Result<MockServer> {
    let server = MockServer::start().await;
    for model in ["model-a", "model-b"] {
        // The endpoint reports the model it actually received and original usage.
        // https://platform.openai.com/docs/api-reference/chat/object
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                json!({"model":model}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"experiment-reply", "object":"chat.completion", "created":0,
                "model":model, "choices":[{"index":0,"message":{"role":"assistant",
                "content":"The parser fix is complete."},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6,
                    "prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
                    "completion_tokens_details":{"reasoning_tokens":0}}
            })))
            .mount(&server)
            .await;
    }
    Ok(server)
}

pub(super) async fn fixture(
    directory: &std::path::Path,
    url: &str,
    ppm: u32,
    cap: u32,
) -> Result<(Fixture, HeaderMap, CredentialIssued)> {
    fixture_profile(directory, url, ppm, cap, false).await
}

#[cfg(unix)]
pub(super) async fn learner_fixture(
    directory: &std::path::Path,
    url: &str,
    ppm: u32,
    cap: u32,
) -> Result<(Fixture, HeaderMap, CredentialIssued)> {
    fixture_profile(directory, url, ppm, cap, true).await
}
async fn fixture_profile(
    directory: &std::path::Path,
    url: &str,
    ppm: u32,
    cap: u32,
    learner: bool,
) -> Result<(Fixture, HeaderMap, CredentialIssued)> {
    let url_db = format!("sqlite://{}", directory.join("experiment.db").display());
    let owner = crate::tenancy::ownership::acquire(&url_db)?;
    let db = crate::db::connect_tenancy(&url_db, &owner).await?;
    let fixture = Fixture::with_database(db, OperationLimits::default(), &mut |_| {}).await?;
    let (mut fixture, mut route, slot) =
        crate::tenancy::tests::execution::provision_protocol_with_fixture(
            fixture,
            url,
            "chat_completions",
            &mut |_| {},
        )
        .await?;
    fixture.service.shutdown().await?;
    route.revision_id = "experiment-r1".try_into()?;
    let policy = ResourceRef {
        owner_kind: bitrouter_ai::tenancy::authority::OwnerKind::Tenant,
        owner_id: fixture.tenant.as_str().try_into()?,
        resource_kind: bitrouter_ai::tenancy::authority::ResourceKind::Policy,
        resource_id: "native-experiment-policy".try_into()?,
        revision_id: "r1".try_into()?,
    };
    let resources = Resources::with_key(fixture.installation.clone(), [29; 32])?;
    let mut manifest = json!({
    "format":1,"installation_id":fixture.installation,"resources":[
      {"reference":policy,"definition":{"kind":"policy","name":"selected","document":signed_experiment("selected", ppm, cap)?}},
      {"reference":route,"definition":{"kind":"route","selector":"bitrouter/observed","policy":policy,
        "routing":{"trajectory":{"enabled":true},"providers":{"mock":{"api_base":url,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model-a"},{"id":"model-b"}]}},
        "routers":{"observed":{"selection":{"kind":"policy","policy":"selected","base_model":"mock:model-a"}}}},
        "credentials":[{"provider":"mock","account":"default","credential":slot}]}}
    ]});
    let mut alias = route.clone();
    alias.resource_id = "learner-alias".try_into()?;
    if learner {
        let mut imported = manifest["resources"][1].clone();
        imported["reference"] = serde_json::to_value(&alias)?;
        manifest["resources"]
            .as_array_mut()
            .context("fixture resources")?
            .push(imported);
    }
    resources
        .provision_bytes(&fixture.db, &serde_json::to_vec(&manifest)?)
        .await?;
    let snapshot = store::load(&fixture.db, Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        Some(owner),
        OperationLimits::default(),
        snapshot,
        Arc::new(resources),
    )
    .await?;
    let mut input = fixture.configuration_input("native-experiment-config")?;
    input.document.routes = vec![route];
    if learner {
        input.document.routes.push(alias);
    }
    input.document.policy = Some(policy);
    input.document.learning.mode = LearningMode::Explore;
    input.document.limits.active_executions = 2;
    let revision = match fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Configuration(value) => value.revision_id,
        _ => anyhow::bail!("expected experiment configuration"),
    };
    let credential = fixture.issue("native-experiment-key").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = crate::tenancy::tests::execution::headers(
        &fixture,
        credential.secret.as_deref().context("secret")?,
        &revision,
    )?;
    Ok((fixture, headers, credential))
}

pub(super) fn task() -> TaskInput {
    let mut value = input();
    value.text = "Fix the parser error in src/parser.rs: expected a closing delimiter.".into();
    value.routing.model = ModelMode::Policy;
    // Small explicit run limits leave genuine terminal room in the shared ACK
    // budget. Host limits and the ordinary NativeSession contract stay unchanged.
    value.limits = Some(bitrouter_ai::core::protocol::Limits {
        active_models: 1,
        agents: 1,
        child_depth: 1,
        model_attempts: 1,
        outstanding_tools: 1,
        total_tools: 1,
        queued_runs: 1,
        mailbox_messages: 1,
        active_seconds: 30,
        checkpoint_bytes: 128 * 1024,
        unacknowledged_bytes: 256 * 1024,
        ephemeral_bytes: 64 * 1024,
        input_bytes: 4096,
    });
    value
}

pub(super) async fn open(
    fixture: &Fixture,
    headers: &HeaderMap,
    url: &str,
    directory: &std::path::Path,
    name: &str,
) -> Result<NativeSession> {
    let workspace = directory.join(name);
    std::fs::create_dir_all(&workspace)?;
    let app = Arc::new(
        host_app(fixture, url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone()),
    );
    Ok(tokio::time::timeout(
        TIMEOUT,
        NativeSession::open_authenticated(
            app,
            headers.clone(),
            name,
            &workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        ),
    )
    .await??)
}

pub(super) async fn freeze(fixture: &Fixture, headers: &HeaderMap) -> Result<EvalSnapshot> {
    let auth = crate::remote_control::auth::ControlAuth::from_lookup(
        &crate::config::ControlConfig {
            credentials: vec![crate::config::ControlCredentialConfig {
                id: "test-operator".into(),
                token_env: "NATIVE_EXPERIMENT_OPERATOR".into(),
                scopes: vec![bitrouter_ai::control::ControlScope::Tenancy],
            }],
            ..Default::default()
        },
        |_| Some("native-experiment-operator-token-at-least-32".into()),
    )?;
    let mut request = Request::post(format!(
        "/control/v1/tenants/{}/evals/snapshots",
        fixture.tenant
    ))
    .body(Body::from(r#"{"frozen_at":"2026-10-26T00:00:00.000Z"}"#))?;
    *request.headers_mut() = headers.clone();
    request
        .headers_mut()
        .insert("content-type", "application/json".parse()?);
    let reply = tokio::time::timeout(
        TIMEOUT,
        crate::tenancy::http::router(fixture.service.clone(), auth).oneshot(request),
    )
    .await??;
    let status = reply.status();
    let body = tokio::time::timeout(
        TIMEOUT,
        axum::body::to_bytes(reply.into_body(), 8 * 1024 * 1024),
    )
    .await??;
    ensure!(
        status == StatusCode::OK,
        "freeze failed {status}: {}",
        String::from_utf8_lossy(&body)
    );
    let result: EvalSnapshot = serde_json::from_slice(&body)?;
    ensure!(result.frozen_at == "2026-10-26T00:00:00.000Z");
    Ok(result)
}

pub(super) async fn records(fixture: &Fixture, kind: &str) -> Result<Vec<Value>> {
    let backend = fixture.db.get_database_backend();
    let query = sea_orm::sea_query::Query::select()
        .column(sea_orm::sea_query::Alias::new("body"))
        .from(sea_orm::sea_query::Alias::new("checkpoint_evolution"))
        .and_where(
            sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new("installation_id"))
                .eq(fixture.installation.as_str()),
        )
        .and_where(
            sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new("tenant_id"))
                .eq(fixture.tenant.as_str()),
        )
        .and_where(sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new("kind")).eq(kind))
        .limit(129)
        .to_owned();
    let rows = fixture.db.query_all(backend.build(&query)).await?;
    ensure!(rows.len() <= 128);
    rows.iter()
        .map(|row| Ok(serde_json::from_str(&row.try_get::<String>("", "body")?)?))
        .collect()
}

pub(super) async fn drained(fixture: &Fixture) -> Result<()> {
    tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
    ensure!(fixture.service.current().is_ok());
    Ok(())
}

pub(super) async fn chain(
    fixture: &Fixture,
    server: &MockServer,
    run: &str,
) -> Result<ExperimentArm> {
    drained(fixture).await?;
    let units = records(fixture, "native_experiment_unit_v1").await?;
    if !units.iter().any(|unit| unit["run"] == run) {
        let pending = records(fixture, "native_experiment_unit_pending_v1").await?;
        let steps = records(fixture, "native_experiment_step_pending_v1").await?;
        let phases: Vec<_> = steps
            .iter()
            .map(|step| {
                (
                    step["receipt"]["state"].clone(),
                    step["receipt"]["dispatch"].clone(),
                    step["publication"]["state"].clone(),
                    step["ack"].is_null(),
                    step["ack"]["planned"].clone(),
                )
            })
            .collect();
        anyhow::bail!(
            "actual run has no completed assignment; completed={} pending={} step_phases={phases:?}",
            units.len(),
            pending.len()
        );
    }
    let unit = units
        .iter()
        .find(|unit| unit["run"] == run)
        .context("actual run has no completed assignment")?;
    ensure!(unit["pending"] == 0 && unit["steps"] == 1);
    let steps = records(fixture, "native_experiment_step_v1").await?;
    let step = steps
        .iter()
        .find(|step| step["unit"] == unit["key"])
        .context("actual step missing")?;
    ensure!(step["facts"]["run"] == run && step["ack"]["planned"] == true);
    ensure!(step["publication"]["state"] == "published");
    let execution = step["execution"]
        .as_str()
        .context("execution")?
        .try_into()?;
    let receipt = crate::tenancy::receipts::store::lookup(
        &fixture.db,
        &fixture.installation,
        &fixture.tenant,
        &execution,
    )
    .await?
    .context("real receipt missing")?;
    ensure!(receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Committed);
    ensure!(
        receipt.execution_revision.as_str()
            == unit["profile"]["revision"].as_str().context("R1")?
    );
    ensure!(receipt.principal_id == fixture.principal && receipt.attempts.len() == 1);
    let arm: ExperimentArm = serde_json::from_value(unit["assignment"]["arm"].clone())?;
    let expected = if arm == ExperimentArm::Challenger {
        "model-b"
    } else {
        "model-a"
    };
    let requests = server
        .received_requests()
        .await
        .context("requests unavailable")?;
    let request = requests.last().context("no actual provider send")?;
    ensure!(serde_json::from_slice::<Value>(&request.body)?["model"] == expected);
    ensure!(
        request
            .headers
            .get("authorization")
            .is_some_and(|v| v == "Bearer owned-upstream-secret")
    );
    // Full original usage is required, not synthetic quality or inferred totals.
    ensure!(receipt.usage.input_tokens == Some(4) && receipt.usage.output_tokens == Some(2));
    ensure!(receipt.usage.provenance == bitrouter_ai::tenancy::execution::Provenance::Provider);
    let attempt = receipt.attempts.first().context("attempt")?;
    ensure!(attempt.target.as_ref().context("target")?.model_id == expected);
    ensure!(attempt.outcome == bitrouter_ai::tenancy::execution::Outcome::Completed);
    ensure!(attempt.native_usage.iter().any(|value| value.name
        == bitrouter_ai::tenancy::execution::UsageDimension::InputTokens
        && value.value == 4));
    ensure!(attempt.native_usage.iter().any(|value| value.name
        == bitrouter_ai::tenancy::execution::UsageDimension::OutputTokens
        && value.value == 2));
    Ok(arm)
}
