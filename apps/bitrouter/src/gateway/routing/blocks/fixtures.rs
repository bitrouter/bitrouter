//! Shared recorded-session and live-upstream fixtures; no learning runtime setup.

use crate::acp_trajectory::{CanonicalStore, RecordingScope};
use crate::config::Config;
use crate::gateway::caller::CallerContext;
use crate::gateway::pipeline::types::PipelineRequest;
use crate::gateway::routing::blocks::allocation::BanditConfig;
use crate::gateway::routing::blocks::control::{BlockDefinition, BlockRule};
use crate::harness::acp::capture::CapturePort;
use crate::policy_lock::{PolicyDefinition, PolicyLock, deterministic_yaml};
use crate::storage::capture::{CaptureDirection, CaptureEvent, CaptureKind};
use crate::storage::session_identity::SessionIdentity;
use anyhow::{Context, Result};
use bitrouter_ai::types::{ApiProtocol, GenerationParams, Message, Prompt, Role};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

pub(crate) struct Fixture {
    pub(crate) assembled: crate::assemble::Assembled,
    pub(crate) upstream: MockServer,
    pub(crate) _home: tempfile::TempDir,
}

#[cfg(feature = "core")]
pub(crate) async fn fixture(named: bool) -> Result<Fixture> {
    fixture_with_persistence(named, false).await
}

pub(crate) async fn fixture_with_persistence(named: bool, persistent: bool) -> Result<Fixture> {
    let upstream = MockServer::start().await;
    for model in ["strong", "cheap"] {
        Mock::given(method("POST")).and(path("/chat/completions"))
            .and(body_partial_json(json!({"model":model})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"fixture-response", "object":"chat.completion", "model":model,
                "choices":[{"index":0,"message":{"role":"assistant","content":"recorded response"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
            }))).mount(&upstream).await;
    }
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"model":"unavailable"})))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_json(json!({"error":{"message":"fixture unavailable"}})),
        )
        .mount(&upstream)
        .await;
    let home = tempfile::tempdir()?;
    let config_path = home.path().join("bitrouter.yaml");
    let mut config: Config = crate::config::parse_with(
        &format!(
            r#"
server:
  skip_auth: true
database:
  url: "sqlite::memory:"
registry:
  enabled: false
policy:
  mode: frozen
providers:
  fixture:
    api_base: "{}"
    api_key: secret-not-for-evidence
    api_protocol:
      - "*": chat_completions
    models:
      - id: strong
        capabilities: [tools]
        pricing:
          input_micro_usd_per_token: 2
          output_micro_usd_per_token: 4
      - id: cheap
        capabilities: [tools]
        pricing:
          input_micro_usd_per_token: 1
          output_micro_usd_per_token: 2
      - id: unavailable
models:
  coding:
    endpoints:
      - provider: fixture
        service_id: strong
  candidate:
    endpoints:
      - provider: fixture
        service_id: cheap
  fallback:
    endpoints:
      - provider: fixture
        service_id: unavailable
      - provider: fixture
        service_id: cheap
presets:
  coding:
    model: "fixture:strong"
    system_prompt: "Use recorded requirements."
  candidate:
    model: "fixture:cheap"
    system_prompt: "Use recorded requirements."
    params:
      temperature: 0.2
"#,
            upstream.uri()
        ),
        |_| None,
    )?;
    if persistent {
        config.database.url = format!(
            "sqlite://{}?mode=rwc",
            home.path().join("gateway.db").display()
        );
    }
    if named {
        let mut lock = PolicyLock::default();
        for (name, default_tier) in [("coding", "strong"), ("candidate", "cheap")] {
            config
                .presets
                .get_mut(name)
                .context("fixture preset missing")?
                .policy = Some(name.into());
            lock.policies.insert(
                name.into(),
                PolicyDefinition {
                    tiers: BTreeMap::from([
                        ("strong".into(), "fixture:strong".into()),
                        ("cheap".into(), "fixture:cheap".into()),
                    ]),
                    default_tier: Some(default_tier.into()),
                    tool_use_tier: Some("strong".into()),
                    tool_safe_tiers: vec!["strong".into()],
                    ..PolicyDefinition::default()
                },
            );
        }
        tokio::fs::write(
            home.path().join("policy-lock.yaml"),
            deterministic_yaml(&lock)?,
        )
        .await?;
    }
    let assembled = crate::assemble::build_app_with_path(&config, Some(&config_path)).await?;
    Ok(Fixture {
        assembled,
        upstream,
        _home: home,
    })
}

pub(crate) async fn session(fixture: &Fixture, id: &str, source: &str) -> Result<SessionIdentity> {
    Ok(recording(fixture, id, source, true).await?.0)
}

pub(crate) async fn recording(
    fixture: &Fixture,
    id: &str,
    source: &str,
    acknowledge: bool,
) -> Result<(SessionIdentity, Arc<crate::acp_trajectory::Recorder>)> {
    let identity = SessionIdentity {
        owner: "local".into(),
        source: source.into(),
        native_session_id: id.into(),
    };
    let recorder = CanonicalStore::new(fixture.assembled.db.clone())
        .recorder(RecordingScope {
            owner: identity.owner.clone(),
            source: identity.source.clone(),
            controller_instance_id: Some("controller".into()),
            route_scope_id: Some("local".into()),
        })
        .await?;
    if acknowledge {
        fixture
            .assembled
            .gateway_requests
            .inventory()
            .register_capture(recorder.connection_id(), "local", "controller")
            .await?;
    }
    for (kind, method, call_id, payload) in [
        (
            CaptureKind::Request,
            "session/new",
            1,
            json!({"cwd":"/fixture"}),
        ),
        (
            CaptureKind::Response,
            "session/new",
            1,
            json!({"result":{"sessionId":id}}),
        ),
        (
            CaptureKind::Request,
            "session/prompt",
            2,
            json!({"sessionId":id,"prompt":[{"type":"text","text":"Make a small change."}]}),
        ),
    ] {
        recorder
            .record(CaptureEvent {
                direction: CaptureDirection::Client,
                kind,
                call_id: Some(call_id),
                method: method.into(),
                payload,
            })
            .await?;
    }
    Ok((identity, recorder))
}

pub(crate) fn definition(selector: &str, challenger: &str) -> BlockDefinition {
    BlockDefinition {
        block_id: "coding-block".into(),
        source: "fixture".into(),
        rationale: "Compare complete compatible coding routes.".into(),
        rules: vec![BlockRule {
            selector: selector.into(),
            fingerprint: None,
            baseline_route: selector.into(),
            challenger_route: challenger.into(),
        }],
        independence_rationale: "One controlled block in this fixture.".into(),
        dependencies: BTreeMap::new(),
        measurement_contract: "fixture-measurement".into(),
        batch_sessions: 32,
        bandit: BanditConfig {
            initial_exposure_ppm: 500_000,
            ..BanditConfig::default()
        },
    }
}

pub(crate) fn request(
    identity: &SessionIdentity,
    id: &str,
    model: &str,
) -> Result<PipelineRequest> {
    let prompt = Prompt {
        model: model.into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "Make a small change.")],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let mut request = PipelineRequest::new(model, CallerContext::local(), prompt);
    request.request_id = id.into();
    request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
    request
        .headers
        .insert("x-bitrouter-controller-id", "controller".parse()?);
    request.headers.insert(
        "x-bitrouter-acp-session-id",
        identity.native_session_id.parse()?,
    );
    Ok(request)
}
