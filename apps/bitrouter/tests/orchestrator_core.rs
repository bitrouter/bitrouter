//! The managed core exercised through the shipped App and its production hooks.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bitrouter_ai::types::Role;
use bitrouter_orchestrator::core::checkpoint::{
    CheckpointAck, CheckpointBatch, DurableHead, sha256,
};
use bitrouter_orchestrator::core::protocol::{
    Bind, Capabilities, ContextMode, CoreError, DiscardableHistory, ErrorCode, HarnessManifest,
    Limits, MaterialRef, OwnershipGrant, RoutingSettings, ServerMessage, SignalUpdate, TaskInput,
};
use bitrouter_orchestrator::core::session::{CoreSession, HarnessPort, RunStatus};
use bitrouter_sdk::caller::CallerContext;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

#[path = "support/decision.rs"]
mod decision;

#[path = "orchestrator_core/native_http.rs"]
mod native_http;

#[path = "orchestrator_core/managed_api.rs"]
mod managed_api;

#[path = "orchestrator_core/auth_scope.rs"]
mod auth_scope;

#[derive(Default)]
struct Store {
    heads_received: usize,
    head: DurableHead,
    batches: Vec<CheckpointBatch>,
    acknowledgements: BTreeMap<String, CheckpointAck>,
}
struct Harness {
    grant: OwnershipGrant,
    store: Mutex<Store>,
}
#[async_trait]
impl HarnessPort for Harness {
    async fn read_artifact(
        &self,
        _reference: &bitrouter_orchestrator::core::protocol::ArtifactRef,
        _offset: u64,
        _max_bytes: u64,
    ) -> std::result::Result<Vec<u8>, CoreError> {
        Err(CoreError::rejected(
            ErrorCode::ArtifactUnavailable,
            "fixture has no artifacts",
        ))
    }

    async fn commit(
        &self,
        batch: CheckpointBatch,
    ) -> std::result::Result<CheckpointAck, CoreError> {
        let mut store = self.store.lock().await;
        let retained = store.acknowledgements.get(&batch.identity.batch_id);
        let ack = batch.validate_append(
            &self.grant,
            &store.head,
            &Limits::default(),
            &BTreeMap::new(),
            retained,
        )?;
        if retained.is_none() {
            store.head = ack.head();
            store
                .acknowledgements
                .insert(batch.identity.batch_id.clone(), ack.clone());
            store.batches.push(batch);
        }
        Ok(ack)
    }
    async fn send(&self, _: ServerMessage) -> std::result::Result<(), CoreError> {
        Err(CoreError::rejected(
            ErrorCode::OperationConflict,
            "fixture has no external tool effects",
        ))
    }
}

fn input(text: &str) -> TaskInput {
    TaskInput {
        max_concurrent_subagents: None,
        text: text.into(),
        model: "fixture-model".into(),
        effort: None,
        max_output_tokens: Some(128),
        context_limit_bytes: None,
        routing: RoutingSettings {
            context: ContextMode::Auto,
            ..Default::default()
        },
        discardable_history: None,
        acceptance_criteria: Vec::new(),
        required_materials: Vec::new(),
        verification: None,
        limits: None,
    }
}

#[tokio::test]
async fn assembled_app_revalidates_rebuilt_context_before_fresh_count_and_execution() -> Result<()>
{
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses/input_tokens"))
        .respond_with(|request: &Request| {
            let text = String::from_utf8_lossy(&request.body);
            let input_tokens = if text.contains("current-independent-task")
                && text.contains("old-history-evidence")
            {
                1000
            } else {
                100
            };
            ResponseTemplate::new(200).set_body_json(
                json!({"object":"response.input_tokens", "input_tokens":input_tokens}),
            )
        })
        .mount(&upstream)
        .await;
    Mock::given(method("POST")).and(path("/v1/responses"))
        .respond_with(|request: &Request| {
            let text = if String::from_utf8_lossy(&request.body).contains("current-independent-task") { "current-done" } else { "old-history-evidence" };
            ResponseTemplate::new(200).set_body_json(json!({
                "id":"resp_fixture", "object":"response", "status":"completed", "model":"counted",
                "output":[{"id":"msg_fixture", "type":"message", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":text,"annotations":[]}]}],
                "usage":{"input_tokens":100,"output_tokens":5,"total_tokens":105,
                    "input_tokens_details":{"cached_tokens":30,"cache_write_tokens":10},
                    "output_tokens_details":{"reasoning_tokens":2}}
            }))
        }).mount(&upstream).await;
    let config = bitrouter_sdk::config::parse_with(
        &format!(
            r#"
inherit_defaults: false
database:
  url: 'sqlite::memory:'
providers:
  fixture:
    api_base: {}/v1
    api_key: fixture-secret
    models:
      - id: counted
        api_protocol: responses
        capabilities: [tools]
        input_token_counting: responses
        pricing:
          input_micro_usd_per_token: 2
          cache_read_micro_usd_per_token: 0.5
          cache_write_micro_usd_per_token: 3
          output_micro_usd_per_token: 4
        token_limits:
          max_input_tokens: 500
          max_output_tokens: 128
          context_window: 1000
models:
  fixture-model:
    endpoints:
      - provider: fixture
        service_id: counted
"#,
            upstream.uri()
        ),
        |_| None,
    )?;
    let assembled = bitrouter::assemble::build_app(&config).await?;
    let db = assembled.db.clone();
    let app = Arc::new(assembled.app);
    let grant = OwnershipGrant {
        session_id: "session_1".into(),
        harness_id: "harness_1".into(),
        core_instance_id: "core_1".into(),
        execution_epoch: 1,
    };
    let harness = Arc::new(Harness {
        grant: grant.clone(),
        store: Mutex::new(Store::default()),
    });
    let manifest = HarnessManifest {
        tool_manifest_digest: HarnessManifest::digest(&[])?,
        tools: Vec::new(),
        workspace_id: "workspace_1".into(),
        workspace_revision: None,
        permission_revision: 1,
        max_tool_output_bytes: 8192,
        artifact_quota_bytes: 1024 * 1024,
        max_artifact_chunk_bytes: 8192,
        required_features: Vec::new(),
    };
    let caps = Capabilities {
        version: 1,
        core_instance_id: "core_1".into(),
        operations: Vec::new(),
        transports: vec!["in_process".into()],
        unsupported_features: Vec::new(),
        limits: Limits::default(),
        max_sessions: 16,
        max_host_model_attempts: 16,
    };
    let session = CoreSession::bind(
        Bind {
            grant,
            durable_head: DurableHead::default(),
            checkpoint: None,
            manifest: manifest.clone(),
            limits: Limits::default(),
        },
        &caps,
        app,
        CallerContext::new("embedded-key", "embedded-owner"),
        harness.clone(),
    )
    .await?;
    session
        .signals(
            "material",
            SignalUpdate {
                signal_revision: 1,
                observed_at: "2026-10-02T12:00:00Z".into(),
                scope: "session_1".into(),
                source: "harness_1".into(),
                workspace_revision: None,
                manifest,
                facts: Default::default(),
                materials: vec![MaterialRef {
                    material_id: "readme".into(),
                    version: "v1".into(),
                    sha256: sha256(b"required-readme"),
                    media_type: "text/plain".into(),
                    provenance: "harness_document".into(),
                    required: true,
                    artifact: None,
                    content: Some("required-readme".into()),
                }],
            },
        )
        .await?;
    session
        .start(
            "first",
            session.head().await.state_revision,
            input("prior-task"),
        )
        .await?;
    let first = session.drive().await?;
    assert_eq!(
        first.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let history = &first.agents[&first.agent_id].history;
    let mut task = input("current-independent-task");
    task.discardable_history = Some(DiscardableHistory {
        history_sha256: sha256(&serde_json::to_vec(history)?),
        message_indices: history
            .iter()
            .enumerate()
            .filter_map(|(index, message)| (message.role == Role::Assistant).then_some(index))
            .collect(),
    });
    session
        .start("second", session.head().await.state_revision, task)
        .await?;
    let final_state = session.drive().await?;
    assert_eq!(
        final_state.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed),
        "{:?}",
        final_state
            .root_turn()
            .and_then(|turn| turn.terminal_reason.as_ref())
    );
    let turn = final_state.root_turn().context("missing final turn")?;
    assert_eq!(turn.steps.len(), 2);
    assert!(turn.steps[0].attempts.is_empty());
    assert_eq!(turn.steps[1].attempts.len(), 1);
    let receipt = turn.steps[1].attempts[0]
        .receipt
        .as_ref()
        .context("missing execution receipt")?;
    assert_eq!(receipt.cost_micro_usd, Some(185));
    assert_eq!(receipt.cost_source, "configured_token_estimate");
    assert_eq!(receipt.report.actual_provider.as_deref(), Some("fixture"));
    assert_eq!(receipt.report.actual_model.as_deref(), Some("counted"));
    assert_eq!(receipt.report.cache.read_tokens, Some(30));
    assert_eq!(receipt.report.cache.write_tokens, Some(10));
    assert_eq!(receipt.cache_observation_source, "responses_usage");
    let run = final_state.run.as_ref().context("missing run")?;
    let accounting = run
        .token_accounting
        .as_ref()
        .context("missing accounting")?;
    assert_eq!(run.model_attempts, 1);
    assert_eq!(
        accounting.complete_estimate_micro_usd(run.model_attempts),
        Some(185)
    );
    assert!(
        turn.steps[1]
            .context_validation
            .as_ref()
            .and_then(|record| record.report.as_ref())
            .is_some_and(|report| report.allowed)
    );
    let requests = upstream
        .received_requests()
        .await
        .context("missing upstream requests")?;
    let paths = requests
        .iter()
        .map(|request| request.url.path())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "/v1/responses/input_tokens",
            "/v1/responses",
            "/v1/responses/input_tokens",
            "/v1/responses/input_tokens",
            "/v1/responses"
        ]
    );
    let counted: Value = serde_json::from_slice(&requests[3].body)?;
    let generated: Value = serde_json::from_slice(&requests[4].body)?;
    assert_eq!(counted["input"], generated["input"]);
    assert!(generated.to_string().contains("required-readme"));
    assert!(!generated.to_string().contains("old-history-evidence"));
    let store = harness.store.lock().await;
    let payloads = store
        .batches
        .iter()
        .map(|batch| batch.decode(&Limits::default()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let kinds = payloads
        .iter()
        .flat_map(|payload| &payload.events)
        .map(|event| event.kind.as_str())
        .collect::<Vec<_>>();
    let rebuild = kinds
        .iter()
        .position(|kind| *kind == "context.rebuild")
        .context("missing rebuild")?;
    assert_eq!(
        &kinds[rebuild + 1..rebuild + 4],
        [
            "context.validation.intent",
            "context.validation.outcome",
            "model.input_count.intent"
        ]
    );
    // One metering record for each logical model request; reconstruction does not
    // register a second request under the original request identity.
    use sea_orm::{ConnectionTrait, Statement};
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT COUNT(*) AS n FROM requests",
        ))
        .await?
        .context("request count")?;
    assert_eq!(row.try_get::<i64>("", "n")?, 2);
    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            "SELECT estimated_charge_micro_usd, charge_status, charge_evidence_json FROM requests",
        ))
        .await?;
    let bitrouter_sdk::language_model::native_accounting::NativeTokenCost::ConfiguredEstimate {
        pricing_version,
        normalized_usage,
        ..
    } = &receipt.report.token_cost
    else {
        anyhow::bail!("missing native estimate");
    };
    for row in rows {
        assert_eq!(row.try_get::<i64>("", "estimated_charge_micro_usd")?, 185);
        assert_eq!(row.try_get::<String>("", "charge_status")?, "computed");
        let evidence: bitrouter::metering::pricing::ChargeEvidence =
            serde_json::from_str(&row.try_get::<String>("", "charge_evidence_json")?)?;
        assert_eq!(&evidence.pricing_version, pricing_version);
        assert_eq!(&evidence.normalized_usage, normalized_usage);
    }
    Ok(())
}

#[tokio::test]
async fn core_uses_shared_semantics_named_policy_and_learn_receipts() -> Result<()> {
    use bitrouter::eval::types::EvalScope;
    use bitrouter::policy_lock::{PolicyDefinition, PolicyLock};
    use bitrouter_orchestrator::core::context_router::FEATURE;
    use bitrouter_orchestrator::core::protocol::ModelMode;
    let home = tempfile::tempdir()?;
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"core-fixture", "object":"chat.completion", "model":"cheap",
            "choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":1,"total_tokens":11}
        }))).mount(&upstream).await;
    let mut lock = PolicyLock::default();
    lock.policies.insert("auto".into(), PolicyDefinition {
        tiers:BTreeMap::from([("strong".into(), "mock:strong".into()), ("cheap".into(), "mock:cheap".into())]),
        routes:BTreeMap::from([("semantic_route/v1|code:generation|implement|normal".into(), "cheap".into())]),
        default_tier:Some("strong".into()),
        tool_use_tier:Some("strong".into()), tool_safe_tiers:vec!["strong".into(),"cheap".into()],
        progress_guard:Some(serde_json::from_value(json!({"escalation_tier":"strong","protected_tiers":["strong"],"max_consecutive_unprotected":10,"hold_for_requests":1,"incomplete_history":"observe"}))?),
        ..PolicyDefinition::default()
    });
    decision::certify_routes(&mut lock);
    std::fs::write(
        home.path().join("policy-lock.yaml"),
        bitrouter::policy_lock::deterministic_yaml(&lock)?,
    )?;
    let mut config = bitrouter_sdk::config::parse_with(
        &format!(
            r#"
inherit_defaults: false
server:
  skip_auth: true
database:
  url: 'sqlite::memory:'
trajectory:
  enabled: true
providers:
  mock:
    api_base: '{}'
    api_key: fixture
    models: [{{id: strong}}, {{id: cheap}}]
routers:
  auto:
    selection: {{kind: policy, policy: auto, base_model: 'mock:strong'}}
policy:
  path: './policy-lock.yaml'
"#,
            upstream.uri()
        ),
        |_| None,
    )?;
    let backend = decision::attach(&mut config, |_| {
        ("code:generation", "implement", "progressing")
    })
    .await?;
    let assembled =
        bitrouter::build_app_with_path(&config, Some(&home.path().join("bitrouter.yaml"))).await?;
    let store = bitrouter::eval::store::EvalStore::new(assembled.db.clone());
    let publisher = assembled
        .trajectory_outbox_publisher
        .clone()
        .context("trajectory publisher")?;
    let app = Arc::new(assembled.app);
    let grant = OwnershipGrant {
        session_id: "session_1".into(),
        harness_id: "harness_1".into(),
        core_instance_id: "core_1".into(),
        execution_epoch: 1,
    };
    let harness = Arc::new(Harness {
        grant: grant.clone(),
        store: Mutex::new(Store::default()),
    });
    let manifest = HarnessManifest {
        tool_manifest_digest: HarnessManifest::digest(&[])?,
        tools: Vec::new(),
        workspace_id: "workspace_1".into(),
        workspace_revision: None,
        permission_revision: 1,
        max_tool_output_bytes: 8192,
        artifact_quota_bytes: 1024 * 1024,
        max_artifact_chunk_bytes: 8192,
        required_features: vec![FEATURE.into()],
    };
    let caps = Capabilities {
        version: 1,
        core_instance_id: "core_1".into(),
        operations: vec![FEATURE.into()],
        transports: vec!["in_process".into()],
        unsupported_features: Vec::new(),
        limits: Limits::default(),
        max_sessions: 16,
        max_host_model_attempts: 16,
    };
    let session = CoreSession::bind(
        Bind {
            grant,
            durable_head: DurableHead::default(),
            checkpoint: None,
            manifest,
            limits: Limits::default(),
        },
        &caps,
        app.clone(),
        CallerContext::local(),
        harness.clone(),
    )
    .await?;
    let mut task = input("Implement a parser");
    task.model = "bitrouter/auto".into();
    task.routing.model = ModelMode::Policy;
    session
        .start("start", session.head().await.state_revision, task)
        .await?;
    let state = session.drive().await?;
    assert_eq!(
        state.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed),
        "{state:?}"
    );
    let turn = state.root_turn().context("root turn")?;
    let plan = turn
        .steps
        .first()
        .and_then(|step| step.plan.as_ref())
        .context("committed plan")?;
    assert_eq!(plan.effective_model, "mock:cheap");
    assert_eq!(state.context_store.decisions.len(), 1);
    assert_eq!(
        upstream
            .received_requests()
            .await
            .context("generation capture")?
            .len(),
        1
    );
    assert_eq!(
        backend
            .received_requests()
            .await
            .context("semantic capture")?
            .len(),
        1
    );
    app.language_model()
        .context("pipeline")?
        .drain_required_pending_settlements()
        .await?;
    assert_eq!(publisher.drain_after_active_worker().await?.failed, 0);
    let subjects = store.list_subjects().await?;
    assert!(
        !subjects
            .iter()
            .any(|subject| subject.scope == EvalScope::Request),
        "tracked requests must not duplicate episode attribution"
    );
    let episode = subjects
        .iter()
        .find(|subject| subject.scope == EvalScope::Episode)
        .context("episode eval")?;
    let assessment = episode
        .evidence
        .iter()
        .find(|item| item.kind == "routing.assessment")
        .context("semantic receipt")?;
    let committed = state
        .context_store
        .decisions
        .values()
        .next()
        .context("Core semantic receipt")?;
    assert_eq!(
        assessment.attributes.get("decision_receipt_id"),
        Some(&committed.decision_id)
    );
    assert_eq!(
        assessment.attributes.get("model").map(String::as_str),
        Some("fixture-v1")
    );
    let selected = episode
        .evidence
        .iter()
        .find(|item| item.kind == "routing.plan")
        .context("plan receipt")?;
    let core_plan = state
        .context_store
        .executions
        .values()
        .next()
        .and_then(|execution| execution.routing.as_ref())
        .context("Core selected plan")?;
    assert_eq!(
        selected.digest,
        bitrouter::eval::types::canonical_digest(core_plan)?
    );
    assert_eq!(
        selected.attributes.get("model").map(String::as_str),
        Some("mock:cheap")
    );
    Ok(())
}
