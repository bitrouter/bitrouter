//! HTTP-level generality and trajectory-inflation regressions for the
//! task-neutral progress control plane.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum_test::TestServer;
use bitrouter::auth::{NewApiKey, db as auth_db, generate};
use bitrouter::eval::store::EvalStore;
use bitrouter::metering::{ChargeStatus, MeteringStore, PricingSource, TimeWindow};
use bitrouter::policy_lock::{PolicyDefinition, PolicyLock, deterministic_yaml};
use bitrouter::trajectory::guard::{IncompleteHistoryAction, ProgressGuardPolicy};
use bitrouter::trajectory::health::reduce;
use bitrouter::trajectory::replay::replay_episode;
use bitrouter::trajectory::store::{EpisodeAudit, TrajectoryStore};
use bitrouter::trajectory::types::{HistoryCompleteness, RequestStatus, TrajectoryEventKind};
use bitrouter_sdk::config;
use bitrouter_sdk::server::{AppState, build_router};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const ASSISTANT_REVIEW_ACTION: &str =
    r#"{"commands":[{"keystrokes":"git status"}],"task_complete":false}"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InboundProtocol {
    Chat,
    Messages,
    Responses,
}

impl InboundProtocol {
    fn endpoint(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Messages => "/v1/messages",
            Self::Responses => "/v1/responses",
        }
    }

    fn persisted_name(self) -> &'static str {
        match self {
            Self::Chat => "chat_completions",
            Self::Messages => "messages",
            Self::Responses => "responses",
        }
    }

    fn fixture(self) -> &'static str {
        match self {
            Self::Chat => include_str!("fixtures/trajectory/complete_chat.jsonl"),
            Self::Messages => include_str!("fixtures/trajectory/complete_messages.jsonl"),
            Self::Responses => include_str!("fixtures/trajectory/complete_responses.jsonl"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct FixtureRequest {
    stage: String,
    body: Value,
}

struct HttpHarness {
    _home: TempDir,
    config_path: PathBuf,
    config: config::Config,
    strong: MockServer,
    economy: MockServer,
    responses_state: Option<Arc<Mutex<NativeResponsesState>>>,
}

impl HttpHarness {
    async fn new(progress_guard: bool) -> anyhow::Result<Self> {
        Self::new_with_upstream(progress_guard, InboundProtocol::Chat).await
    }

    async fn streaming_responses() -> anyhow::Result<Self> {
        Self::streaming_responses_with_trajectory(true).await
    }

    async fn streaming_responses_with_trajectory(enabled: bool) -> anyhow::Result<Self> {
        let mut harness = Self::new_with_upstream(enabled, InboundProtocol::Responses).await?;
        harness.config.trajectory.enabled = enabled;
        Ok(harness)
    }

    async fn streaming_responses_split_authority() -> anyhow::Result<Self> {
        Self::new_with_upstream_authority(true, InboundProtocol::Responses, false).await
    }

    async fn new_with_upstream(
        progress_guard: bool,
        upstream_protocol: InboundProtocol,
    ) -> anyhow::Result<Self> {
        Self::new_with_upstream_authority(progress_guard, upstream_protocol, true).await
    }

    async fn new_with_upstream_authority(
        progress_guard: bool,
        upstream_protocol: InboundProtocol,
        shared_responses_authority: bool,
    ) -> anyhow::Result<Self> {
        let (economy_route, strong_route) =
            if upstream_protocol == InboundProtocol::Responses && shared_responses_authority {
                ("responses:economy-model", "responses:strong-model")
            } else {
                ("economy:economy-model", "strong:strong-model")
            };
        let policy = PolicyDefinition {
            tiers: BTreeMap::from([
                ("economy".into(), economy_route.into()),
                ("strong".into(), strong_route.into()),
            ]),
            default_tier: Some("economy".into()),
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: vec!["economy".into(), "strong".into()],
            progress_guard: progress_guard.then(|| ProgressGuardPolicy {
                escalation_tier: "strong".into(),
                protected_tiers: BTreeSet::from(["strong".into()]),
                max_consecutive_unprotected: Some(3),
                max_same_projection_unprotected: None,
                max_recovery_count: Some(1),
                max_episode_requests: None,
                max_episode_elapsed_ms: None,
                max_episode_cost_micro_usd: None,
                hold_for_requests: 2,
                incomplete_history: IncompleteHistoryAction::Observe,
            }),
            ..PolicyDefinition::default()
        };
        let mut lock = PolicyLock::default();
        lock.policies.insert("auto".into(), policy);
        Self::with_lock_and_upstream_authority(lock, upstream_protocol, shared_responses_authority)
            .await
    }

    async fn with_lock(lock: PolicyLock) -> anyhow::Result<Self> {
        Self::with_lock_and_upstream(lock, InboundProtocol::Chat).await
    }

    async fn with_lock_and_upstream(
        lock: PolicyLock,
        upstream_protocol: InboundProtocol,
    ) -> anyhow::Result<Self> {
        Self::with_lock_and_upstream_authority(lock, upstream_protocol, true).await
    }

    async fn with_lock_and_upstream_authority(
        lock: PolicyLock,
        upstream_protocol: InboundProtocol,
        shared_responses_authority: bool,
    ) -> anyhow::Result<Self> {
        let home = tempfile::tempdir()?;
        let responses_state = (upstream_protocol == InboundProtocol::Responses)
            .then(|| Arc::new(Mutex::new(NativeResponsesState::default())));
        let strong =
            mock_upstream("strong-model", upstream_protocol, responses_state.clone()).await;
        let economy =
            mock_upstream("economy-model", upstream_protocol, responses_state.clone()).await;
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            home.path().join("trajectory.db").display()
        );
        let (providers_yaml, preset_model) =
            if upstream_protocol == InboundProtocol::Responses && shared_responses_authority {
                (
                    format!(
                        r#"providers:
  responses:
    api_base: "{}"
    api_key: test-key
    api_protocol:
      - "*": responses
    models:
      - id: economy-model
      - id: strong-model"#,
                        economy.uri()
                    ),
                    "responses:strong-model",
                )
            } else {
                (
                    format!(
                        r#"providers:
  strong:
    api_base: "{}"
    api_key: test-key
    api_protocol:
      - "*": {}
    models:
      - id: strong-model
  economy:
    api_base: "{}"
    api_key: test-key
    api_protocol:
      - "*": {}
    models:
      - id: economy-model
  balanced:
    api_base: "{}"
    api_key: test-key
    api_protocol:
      - "*": {}
    models:
      - id: balanced-model"#,
                        strong.uri(),
                        upstream_protocol.persisted_name(),
                        economy.uri(),
                        upstream_protocol.persisted_name(),
                        economy.uri(),
                        upstream_protocol.persisted_name(),
                    ),
                    "strong:strong-model",
                )
            };
        let yaml = format!(
            r#"
server:
  listen: "127.0.0.1:0"
  skip_auth: false
database:
  url: "{database_url}"
trajectory:
  enabled: true
  retention_days: 30
  outbox_batch_size: 10
policy:
  path: "./policy-lock.yaml"
  mode: frozen
{providers_yaml}
presets:
  auto:
    model: "{preset_model}"
    policy: auto
  flex:
    model: "{preset_model}"
    policy: auto
"#,
        );
        let config = config::parse_with(&yaml, |_| None)?;
        write_policy_lock(home.path(), &lock).await?;
        Ok(Self {
            config_path: home.path().join("bitrouter.yaml"),
            config,
            _home: home,
            strong,
            economy,
            responses_state,
        })
    }

    async fn with_guarded_missing_route() -> anyhow::Result<Self> {
        let policy = PolicyDefinition {
            tiers: BTreeMap::from([
                ("economy".into(), "missing:absent-model".into()),
                ("strong".into(), "strong:strong-model".into()),
            ]),
            default_tier: Some("economy".into()),
            progress_guard: Some(ProgressGuardPolicy {
                escalation_tier: "strong".into(),
                protected_tiers: BTreeSet::from(["strong".into()]),
                max_consecutive_unprotected: Some(3),
                max_same_projection_unprotected: None,
                max_recovery_count: Some(1),
                max_episode_requests: None,
                max_episode_elapsed_ms: None,
                max_episode_cost_micro_usd: None,
                hold_for_requests: 2,
                incomplete_history: IncompleteHistoryAction::Observe,
            }),
            ..PolicyDefinition::default()
        };
        let mut lock = PolicyLock::default();
        lock.policies.insert("auto".into(), policy);
        Self::with_lock(lock).await
    }

    async fn assemble(&self) -> anyhow::Result<bitrouter::Assembled> {
        bitrouter::build_app_with_path(&self.config, Some(&self.config_path)).await
    }
}

async fn mock_upstream(
    model: &str,
    protocol: InboundProtocol,
    responses_state: Option<Arc<Mutex<NativeResponsesState>>>,
) -> MockServer {
    let server = MockServer::start().await;
    match protocol {
        InboundProtocol::Responses => {
            Mock::given(method("POST"))
                .and(path("/responses"))
                .respond_with(NativeResponsesStream {
                    model: model.to_owned(),
                    state: responses_state.expect("Responses mock requires shared state"),
                })
                .mount(&server)
                .await;
        }
        InboundProtocol::Chat | InboundProtocol::Messages => {
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(NativeIdResponse {
                    model: model.to_owned(),
                })
                .mount(&server)
                .await;
        }
    }
    server
}

struct NativeIdResponse {
    model: String,
}

impl Respond for NativeIdResponse {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let request_id = request
            .headers
            .get("x-bitrouter-request-id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("missing-request-id");
        ResponseTemplate::new(200).set_body_json(json!({
            "id": format!("upstream-{request_id}"),
            "object": "chat.completion",
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": ASSISTANT_REVIEW_ACTION},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        }))
    }
}

struct NativeResponsesStream {
    model: String,
    state: Arc<Mutex<NativeResponsesState>>,
}

#[derive(Default)]
struct NativeResponsesState {
    next_id: usize,
    issued: BTreeSet<String>,
    forwarded_parents: Vec<Option<String>>,
    served_models: Vec<String>,
}

impl Respond for NativeResponsesStream {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body = match serde_json::from_slice::<Value>(&request.body) {
            Ok(body) => body,
            Err(error) => {
                return ResponseTemplate::new(400)
                    .set_body_string(format!("invalid Responses request: {error}"));
            }
        };
        let previous_response_id = body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(self.model.as_str())
            .to_owned();
        let mut state = self.state.lock().expect("Responses mock state poisoned");
        state.forwarded_parents.push(previous_response_id.clone());
        state.served_models.push(model.clone());
        if previous_response_id
            .as_ref()
            .is_some_and(|parent| !state.issued.contains(parent))
        {
            return ResponseTemplate::new(409).set_body_json(json!({
                "error": {
                    "type": "invalid_request_error",
                    "message": "previous_response_id was not issued by this provider"
                }
            }));
        }
        let upstream_id = format!("provider-only-response-{}", state.next_id);
        state.next_id += 1;
        state.issued.insert(upstream_id.clone());
        drop(state);
        let item = json!({
            "id": format!("provider-item-{upstream_id}"),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": ASSISTANT_REVIEW_ACTION}]
        });
        let response = json!({
            "id": upstream_id,
            "object": "response",
            "status": "completed",
            "model": model,
            "output": [item],
            "usage": {"input_tokens": 11, "output_tokens": 7, "total_tokens": 18}
        });
        let events = [
            json!({
                "type": "response.created",
                "sequence_number": 0,
                "response": {
                    "id": upstream_id,
                    "object": "response",
                    "status": "in_progress",
                    "model": model,
                    "output": []
                }
            }),
            json!({
                "type": "response.output_item.added",
                "sequence_number": 1,
                "output_index": 0,
                "item": {
                    "id": format!("provider-item-{upstream_id}"),
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": []
                }
            }),
            json!({
                "type": "response.content_part.added",
                "sequence_number": 2,
                "item_id": format!("provider-item-{upstream_id}"),
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": ""}
            }),
            json!({
                "type": "response.output_text.delta",
                "sequence_number": 3,
                "item_id": format!("provider-item-{upstream_id}"),
                "output_index": 0,
                "content_index": 0,
                "delta": ASSISTANT_REVIEW_ACTION
            }),
            json!({
                "type": "response.output_text.done",
                "sequence_number": 4,
                "item_id": format!("provider-item-{upstream_id}"),
                "output_index": 0,
                "content_index": 0,
                "text": ASSISTANT_REVIEW_ACTION
            }),
            json!({
                "type": "response.content_part.done",
                "sequence_number": 5,
                "item_id": format!("provider-item-{upstream_id}"),
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": ASSISTANT_REVIEW_ACTION}
            }),
            json!({
                "type": "response.output_item.done",
                "sequence_number": 6,
                "output_index": 0,
                "item": item
            }),
            json!({
                "type": "response.completed",
                "sequence_number": 7,
                "response": response
            }),
        ];
        let body = events
            .iter()
            .map(|event| {
                let event_type = event["type"].as_str().unwrap_or("message");
                format!("event: {event_type}\ndata: {event}\n\n")
            })
            .collect::<String>();
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }
}

async fn write_policy_lock(home: &Path, lock: &PolicyLock) -> anyhow::Result<()> {
    tokio::fs::write(home.join("policy-lock.yaml"), deterministic_yaml(lock)?).await?;
    Ok(())
}

fn server(assembled: &bitrouter::Assembled) -> TestServer {
    let state = AppState {
        language_model: assembled
            .app
            .language_model()
            .expect("language model configured")
            .clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    };
    TestServer::new(build_router(state))
}

async fn add_owner(db: &DatabaseConnection, owner: &str) -> anyhow::Result<String> {
    auth_db::upsert_user(db, owner).await?;
    let generated = generate();
    auth_db::insert_api_key(
        db,
        &NewApiKey {
            id: format!("key-{owner}"),
            key_hash: generated.hash,
            user_id: owner.to_owned(),
            spend_limit_micro_usd: None,
            rpm_limit: None,
            policy_id: None,
        },
    )
    .await?;
    Ok(generated.secret)
}

fn fixture_requests(raw: &str) -> anyhow::Result<Vec<FixtureRequest>> {
    raw.lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

async fn post_fixture(
    server: &TestServer,
    protocol: InboundProtocol,
    bearer: &str,
    fixture: &FixtureRequest,
    previous_response_id: Option<&str>,
) -> anyhow::Result<String> {
    let mut body = fixture.body.clone();
    if protocol == InboundProtocol::Responses {
        if let Some(previous_response_id) = previous_response_id {
            body["previous_response_id"] = Value::String(previous_response_id.to_owned());
        } else if let Some(object) = body.as_object_mut() {
            object.remove("previous_response_id");
        }
    }
    post_body(server, protocol, bearer, &fixture.stage, &body, &[]).await
}

async fn post_body(
    server: &TestServer,
    protocol: InboundProtocol,
    bearer: &str,
    stage: &str,
    body: &Value,
    headers: &[(&str, &str)],
) -> anyhow::Result<String> {
    let mut request = server
        .post(protocol.endpoint())
        .add_header("authorization", format!("Bearer {bearer}"));
    for (name, value) in headers {
        request = request.add_header(*name, *value);
    }
    let response = request.json(body).await;
    assert_eq!(
        response.status_code().as_u16(),
        200,
        "{} request failed: {}",
        stage,
        response.text()
    );
    let response: Value = response.json();
    response["id"]
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{stage} response has no native id"))
}

async fn post_streaming_responses(
    server: &TestServer,
    bearer: &str,
    input: &str,
    previous_response_id: Option<&str>,
) -> anyhow::Result<String> {
    let mut body = json!({"model": "@auto", "stream": true, "input": input});
    if let Some(previous_response_id) = previous_response_id {
        body["previous_response_id"] = Value::String(previous_response_id.to_owned());
    }
    let response = server
        .post(InboundProtocol::Responses.endpoint())
        .add_header("authorization", format!("Bearer {bearer}"))
        .json(&body)
        .await;
    assert_eq!(
        response.status_code().as_u16(),
        200,
        "streaming Responses request failed: {}",
        response.text()
    );
    let body = response.text();
    let events = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let terminal = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .ok_or_else(|| anyhow::anyhow!("stream omitted response.completed: {body}"))?;
    let terminal_id = terminal["response"]["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("terminal response omitted id: {terminal}"))?;
    let response_ids = events
        .iter()
        .filter_map(|event| event["response"]["id"].as_str())
        .collect::<Vec<_>>();
    assert!(
        !response_ids.is_empty(),
        "stream omitted lifecycle response ids"
    );
    assert!(
        response_ids.iter().all(|id| *id == terminal_id),
        "one stream exposed multiple continuation identities: {response_ids:?}"
    );
    Ok(terminal_id.to_owned())
}

async fn trajectory_durable_surfaces(
    db: &DatabaseConnection,
    owner: &str,
) -> anyhow::Result<Vec<String>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT event_json AS value FROM trajectory_events WHERE owner_user_id = ? \
             UNION ALL SELECT payload_json AS value FROM trajectory_outbox WHERE owner_user_id = ? \
             UNION ALL SELECT request_id AS value FROM trajectory_requests WHERE owner_user_id = ? \
             UNION ALL SELECT COALESCE(native_parent_id, '') AS value FROM trajectory_requests WHERE owner_user_id = ? \
             UNION ALL SELECT COALESCE(latest_request_id, '') AS value FROM trajectory_episodes WHERE owner_user_id = ? \
             UNION ALL SELECT subject_json AS value FROM eval_subjects WHERE owner_user_id = ? \
             UNION ALL SELECT result_json AS value FROM eval_results WHERE owner_user_id = ?",
            [
                owner.into(),
                owner.into(),
                owner.into(),
                owner.into(),
                owner.into(),
                owner.into(),
                owner.into(),
            ],
        ))
        .await?;
    rows.into_iter()
        .map(|row| row.try_get("", "value").map_err(Into::into))
        .collect()
}

async fn continuation_durable_surfaces(db: &DatabaseConnection) -> anyhow::Result<Vec<String>> {
    let rows = db
        .query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT continuation_identity || owner_identity || COALESCE(ciphertext, '') || \
             COALESCE(nonce, '') || target_fingerprint || key_id AS value \
             FROM provider_continuations"
                .to_owned(),
        ))
        .await?;
    rows.into_iter()
        .map(|row| row.try_get("", "value").map_err(Into::into))
        .collect()
}

async fn table_row_count(db: &DatabaseConnection, table: &str) -> anyhow::Result<i64> {
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!("SELECT COUNT(*) AS count FROM {table}"),
        ))
        .await?
        .ok_or_else(|| anyhow::anyhow!("{table} count query returned no row"))?;
    Ok(row.try_get("", "count")?)
}

async fn owner_episode_ids(db: &DatabaseConnection, owner: &str) -> anyhow::Result<Vec<String>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT episode_id FROM trajectory_episodes WHERE owner_user_id = ? ORDER BY episode_id",
            [owner.into()],
        ))
        .await?;
    rows.into_iter()
        .map(|row| row.try_get("", "episode_id").map_err(Into::into))
        .collect()
}

async fn request_diagnostics(
    db: &DatabaseConnection,
    owner: &str,
) -> anyhow::Result<Vec<(String, Option<String>)>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT protocol, native_parent_id FROM trajectory_requests WHERE owner_user_id = ? ORDER BY rowid",
            [owner.into()],
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("", "protocol")?,
                row.try_get("", "native_parent_id")?,
            ))
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct NormalizedOutcome {
    completeness: HistoryCompleteness,
    request_count: u64,
    settled_request_count: u64,
    recovery_count: u64,
    latest_projection: Option<String>,
    same_projection_streak: u64,
    same_selected_tier_streak: u64,
    consecutive_unprotected_requests: u64,
    active_hold_remaining: u64,
    selected_tiers: Vec<String>,
    correlation_sources: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct ProgressView<'a> {
    completeness: HistoryCompleteness,
    request_count: u64,
    settled_request_count: u64,
    recovery_count: u64,
    latest_projection: &'a Option<String>,
    same_projection_streak: u64,
    same_selected_tier_streak: u64,
    consecutive_unprotected_requests: u64,
    active_hold_remaining: u64,
    selected_tiers: &'a [String],
}

impl NormalizedOutcome {
    fn progress_view(&self) -> ProgressView<'_> {
        ProgressView {
            completeness: self.completeness,
            request_count: self.request_count,
            settled_request_count: self.settled_request_count,
            recovery_count: self.recovery_count,
            latest_projection: &self.latest_projection,
            same_projection_streak: self.same_projection_streak,
            same_selected_tier_streak: self.same_selected_tier_streak,
            consecutive_unprotected_requests: self.consecutive_unprotected_requests,
            active_hold_remaining: self.active_hold_remaining,
            selected_tiers: &self.selected_tiers,
        }
    }
}

async fn normalized_outcome(
    db: &DatabaseConnection,
    owner: &str,
) -> anyhow::Result<NormalizedOutcome> {
    let episodes = owner_episode_ids(db, owner).await?;
    if episodes.len() != 1 {
        anyhow::bail!(
            "owner {owner} has {} episodes, expected one",
            episodes.len()
        );
    }
    let store = TrajectoryStore::new(db.clone());
    let events = store.events_for_episode(owner, &episodes[0]).await?;
    let selected_tiers = events
        .iter()
        .filter(|event| event.kind == TrajectoryEventKind::RouteIntentRecorded)
        .filter_map(|event| {
            event
                .evidence
                .categorical
                .get("route.selected_tier")
                .cloned()
        })
        .collect();
    let correlation_sources = events
        .iter()
        .filter(|event| event.kind == TrajectoryEventKind::RequestStarted)
        .filter_map(|event| {
            event
                .evidence
                .categorical
                .get("correlation.source")
                .cloned()
        })
        .collect();
    let snapshot = replay_episode(
        &store,
        owner,
        &episodes[0],
        &BTreeSet::from(["strong".to_owned()]),
    )
    .await?;
    Ok(NormalizedOutcome {
        completeness: snapshot.health.completeness,
        request_count: snapshot.health.request_count,
        settled_request_count: snapshot.health.settled_request_count,
        recovery_count: snapshot.health.recovery_count,
        latest_projection: snapshot.health.latest_projection,
        same_projection_streak: snapshot.health.same_projection_streak,
        same_selected_tier_streak: snapshot.health.same_selected_tier_streak,
        consecutive_unprotected_requests: snapshot.health.consecutive_unprotected_requests,
        active_hold_remaining: snapshot.active_hold_remaining,
        selected_tiers,
        correlation_sources,
    })
}

fn replace_fixture_value(value: &mut Value, replacements: &[(&str, &str)]) {
    match value {
        Value::String(text) => {
            for (from, to) in replacements {
                *text = text.replace(from, to);
            }
        }
        Value::Array(values) => {
            for value in values {
                replace_fixture_value(value, replacements);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                replace_fixture_value(value, replacements);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

async fn events_for_only_episode(
    db: &DatabaseConnection,
    owner: &str,
) -> anyhow::Result<Vec<bitrouter::trajectory::types::TrajectoryEvent>> {
    let episodes = owner_episode_ids(db, owner).await?;
    if episodes.len() != 1 {
        anyhow::bail!(
            "owner {owner} has {} episodes, expected one",
            episodes.len()
        );
    }
    TrajectoryStore::new(db.clone())
        .events_for_episode(owner, &episodes[0])
        .await
}

async fn trajectory_request_statuses(
    db: &DatabaseConnection,
    owner: &str,
) -> anyhow::Result<Vec<String>> {
    db.query_all(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT status FROM trajectory_requests WHERE owner_user_id = ? ORDER BY request_id",
        [owner.into()],
    ))
    .await?
    .into_iter()
    .map(|row| row.try_get("", "status").map_err(Into::into))
    .collect()
}

async fn post_guarded_missing_route(server: &TestServer, bearer: &str) -> anyhow::Result<()> {
    let response = server
        .post(InboundProtocol::Chat.endpoint())
        .add_header("authorization", format!("Bearer {bearer}"))
        .add_header("x-bitrouter-request-id", "missing-route-request")
        .json(&json!({
            "model": "@auto",
            "messages": [{"role": "user", "content": "inspect the repository"}]
        }))
        .await;
    assert_eq!(response.status_code().as_u16(), 404);
    let body: Value = response.json();
    assert_eq!(body["error"]["code"], "not_found");
    assert_eq!(
        body["error"]["message"],
        "not found: no active provider declares model 'missing:absent-model'"
    );
    Ok(())
}

async fn trajectory_outbox_state(
    db: &DatabaseConnection,
    owner: &str,
) -> anyhow::Result<(i64, i64)> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS total, \
             COALESCE(SUM(CASE WHEN delivered_at IS NOT NULL THEN 1 ELSE 0 END), 0) AS delivered \
             FROM trajectory_outbox WHERE owner_user_id = ?",
            [owner.into()],
        ))
        .await?
        .ok_or_else(|| anyhow::anyhow!("trajectory outbox count row missing"))?;
    Ok((row.try_get("", "total")?, row.try_get("", "delivered")?))
}

fn assert_json_keys_absent(value: &Value, forbidden: &[&str]) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                assert!(!forbidden.contains(&key.as_str()), "unexpected key {key}");
                assert_json_keys_absent(value, forbidden);
            }
        }
        Value::Array(values) => {
            for value in values {
                assert_json_keys_absent(value, forbidden);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn guarded_named_policy_routing_failure_is_terminally_settled() -> anyhow::Result<()> {
    let harness = HttpHarness::with_guarded_missing_route().await?;
    let assembled = harness.assemble().await?;
    let first_server = server(&assembled);
    let owner = "missing-route-owner";
    let bearer = add_owner(&assembled.db, owner).await?;

    post_guarded_missing_route(&first_server, &bearer).await?;
    let publisher = assembled
        .trajectory_outbox_publisher
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("trajectory publisher missing"))?;
    assert_eq!(publisher.drain_after_active_worker().await?.failed, 0);
    assert_eq!(publisher.drain_after_active_worker().await?.attempted, 0);

    let store = TrajectoryStore::new(assembled.db.clone());
    let events = events_for_only_episode(&assembled.db, owner).await?;
    let trajectory_request_id = events
        .iter()
        .find_map(|event| event.request_id.clone())
        .ok_or_else(|| anyhow::anyhow!("trajectory request identity missing"))?;
    assert_eq!(
        store
            .request(owner, &trajectory_request_id)
            .await?
            .map(|request| request.status),
        Some(RequestStatus::Failed)
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == TrajectoryEventKind::RequestSettled)
            .count(),
        1
    );
    let terminal = events
        .iter()
        .find(|event| event.kind == TrajectoryEventKind::RequestSettled)
        .ok_or_else(|| anyhow::anyhow!("terminal trajectory event missing"))?;
    assert_eq!(
        terminal.evidence.categorical.get("settlement.outcome"),
        Some(&"failed".to_owned())
    );
    assert_eq!(
        terminal.evidence.categorical.get("settlement.usage_origin"),
        Some(&"unknown".to_owned())
    );
    assert_eq!(
        terminal.evidence.categorical.get("settlement.error_code"),
        Some(&"not_found".to_owned())
    );
    assert!(
        !terminal
            .evidence
            .categorical
            .contains_key("settlement.provider")
    );
    assert!(
        !terminal
            .evidence
            .categorical
            .contains_key("settlement.model")
    );
    for key in [
        "settlement.prompt_tokens",
        "settlement.completion_tokens",
        "settlement.reasoning_tokens",
        "settlement.cache_read_tokens",
        "settlement.cache_write_tokens",
        "settlement.total_tokens",
        "settlement.cost_micro_usd",
    ] {
        assert!(!terminal.evidence.structural.contains_key(key));
    }
    assert_eq!(trajectory_outbox_state(&assembled.db, owner).await?, (1, 1));
    assert!(store.pending_outbox(owner).await?.is_empty());
    let outbox_payload: String = assembled
        .db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT payload_json FROM trajectory_outbox WHERE owner_user_id = ?",
            [owner.into()],
        ))
        .await?
        .ok_or_else(|| anyhow::anyhow!("trajectory outbox payload row missing"))?
        .try_get("", "payload_json")?;
    assert_json_keys_absent(
        &serde_json::from_str(&outbox_payload)?,
        &[
            "provider",
            "model",
            "prompt_tokens",
            "completion_tokens",
            "reasoning_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "total_tokens",
            "cost_micro_usd",
            "cost.usd_micros",
            "trajectory.total_tokens",
            "trajectory.cost.usd_micros",
        ],
    );

    let metering_store = MeteringStore::new(assembled.db.clone());
    let usage = metering_store.export_usage(TimeWindow::ThisMonth).await?;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].status.as_deref(), Some("failed"));
    assert_eq!(
        usage[0].usage_origin,
        bitrouter_sdk::language_model::UsageOrigin::Unknown
    );
    assert_eq!(usage[0].raw_usage, None);
    assert_eq!(usage[0].final_charge_micro_usd, None);
    assert_eq!(usage[0].charge_status, ChargeStatus::Unknown);
    assert!(usage[0].provider_id.is_empty());
    assert!(usage[0].model_id.is_empty());
    assert_eq!(usage[0].prompt_tokens, 0);
    assert_eq!(usage[0].completion_tokens, 0);
    assert_eq!(usage[0].reasoning_tokens, 0);
    assert_eq!(usage[0].uncached_input_tokens, 0);
    assert_eq!(usage[0].cache_read_tokens, 0);
    assert_eq!(usage[0].cache_write_tokens, 0);
    assert_eq!(usage[0].output_tokens, 0);
    let charge_evidence = usage[0]
        .charge_evidence
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("unknown charge evidence missing"))?;
    assert_eq!(charge_evidence.status, ChargeStatus::Unknown);
    assert_eq!(charge_evidence.charge_micro_usd, None);
    assert_eq!(charge_evidence.pricing_source, PricingSource::Unknown);
    assert_eq!(
        charge_evidence.unknown_reason.as_deref(),
        Some("usage_unavailable")
    );
    assert_eq!(
        metering_store
            .get_enforceable_spend(&format!("key-{owner}"), TimeWindow::ThisMonth)
            .await?,
        None
    );

    let eval_store = EvalStore::new(assembled.db.clone());
    assert_eq!(eval_store.list_subjects_for_owner(owner).await?.len(), 1);
    let admissions = eval_store.latest_admissions_for_owner(owner).await?;
    assert_eq!(admissions.len(), 1);
    let result_id = admissions
        .keys()
        .next()
        .ok_or_else(|| anyhow::anyhow!("trajectory evaluation admission missing"))?;
    let evaluation = eval_store
        .result(result_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("trajectory evaluation result missing"))?;
    assert_json_keys_absent(
        &serde_json::to_value(&evaluation)?,
        &[
            "provider",
            "model",
            "prompt_tokens",
            "completion_tokens",
            "reasoning_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "total_tokens",
            "cost_micro_usd",
        ],
    );
    assert!(
        !evaluation
            .result
            .metrics
            .contains_key("trajectory.total_tokens")
    );
    assert!(
        !evaluation
            .result
            .metrics
            .contains_key("trajectory.cost.usd_micros")
    );
    assert!(!evaluation.result.metrics.contains_key("cost.usd_micros"));
    let event_count = u64::try_from(events.len())?;

    drop(first_server);
    drop(assembled);

    let restarted = harness.assemble().await?;
    let restarted_server = server(&restarted);
    post_guarded_missing_route(&restarted_server, &bearer).await?;
    let restarted_publisher = restarted
        .trajectory_outbox_publisher
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("restarted trajectory publisher missing"))?;
    assert_eq!(
        restarted_publisher
            .drain_after_active_worker()
            .await?
            .attempted,
        0
    );
    assert_eq!(
        restarted_publisher
            .drain_after_active_worker()
            .await?
            .attempted,
        0
    );
    assert_eq!(
        trajectory_request_statuses(&restarted.db, owner).await?,
        ["failed"]
    );
    let restarted_events = events_for_only_episode(&restarted.db, owner).await?;
    assert_eq!(restarted_events.len(), events.len());
    assert_eq!(trajectory_outbox_state(&restarted.db, owner).await?, (1, 1));
    assert_eq!(
        EvalStore::new(restarted.db.clone())
            .list_subjects_for_owner(owner)
            .await?
            .len(),
        1
    );
    assert_eq!(
        MeteringStore::new(restarted.db.clone())
            .export_usage(TimeWindow::ThisMonth)
            .await?
            .len(),
        1
    );

    let restarted_store = TrajectoryStore::new(restarted.db.clone());
    let pruned = restarted_store
        .prune_before("2999-01-01T00:00:00Z", false, 10)
        .await?;
    assert_eq!(pruned.delivered_outbox_rows, 1);
    assert_eq!(pruned.episode_rows, 1);
    assert_eq!(pruned.request_rows, 1);
    assert_eq!(pruned.event_rows, event_count);
    assert!(owner_episode_ids(&restarted.db, owner).await?.is_empty());
    assert!(
        trajectory_request_statuses(&restarted.db, owner)
            .await?
            .is_empty()
    );
    assert_eq!(trajectory_outbox_state(&restarted.db, owner).await?, (0, 0));
    Ok(())
}

#[tokio::test]
async fn header_free_chat_messages_and_responses_have_equivalent_progress() -> anyhow::Result<()> {
    let harness = HttpHarness::new(true).await?;
    let assembled = harness.assemble().await?;
    let server = server(&assembled);
    let cases = [
        (InboundProtocol::Chat, "protocol-chat"),
        (InboundProtocol::Messages, "protocol-messages"),
        (InboundProtocol::Responses, "protocol-responses"),
    ];
    let mut outcomes = Vec::new();

    for (protocol, owner) in cases {
        let bearer = add_owner(&assembled.db, owner).await?;
        let requests = fixture_requests(protocol.fixture())?;
        let mut response_ids = Vec::new();
        for request in &requests {
            // This matrix intentionally uses a Chat-only upstream. A reserved
            // Responses id has no provider-native mapping in that topology,
            // so subsequent turns exercise canonical history correlation.
            let response_id = post_fixture(&server, protocol, &bearer, request, None).await?;
            response_ids.push(response_id);
        }

        let diagnostics = request_diagnostics(&assembled.db, owner).await?;
        assert_eq!(diagnostics.len(), 3);
        assert!(
            diagnostics
                .iter()
                .all(|(name, _)| name == protocol.persisted_name())
        );
        if protocol == InboundProtocol::Responses {
            assert_eq!(
                response_ids.iter().collect::<BTreeSet<_>>().len(),
                response_ids.len(),
                "Responses must return a distinct native id for every request"
            );
            assert!(diagnostics.iter().all(|(_, parent)| parent.is_none()));
        } else {
            assert!(diagnostics.iter().all(|(_, parent)| parent.is_none()));
        }
        outcomes.push((protocol, normalized_outcome(&assembled.db, owner).await?));
    }

    let chat = &outcomes[0].1;
    for (_, outcome) in &outcomes[1..] {
        assert_eq!(outcome.progress_view(), chat.progress_view());
    }
    assert_eq!(
        outcomes[0].1.correlation_sources,
        ["explicit_root", "canonical_prefix", "canonical_prefix"]
    );
    assert_eq!(
        outcomes[2].1.correlation_sources,
        ["explicit_root", "canonical_prefix", "canonical_prefix"]
    );
    assert_eq!(
        chat.selected_tiers,
        ["economy", "strong", "strong"],
        "recovery escalates immediately and the first review remains held"
    );
    assert_eq!(chat.completeness, HistoryCompleteness::Complete);
    assert_eq!(chat.request_count, 3);
    assert_eq!(chat.settled_request_count, 3);
    assert_eq!(chat.recovery_count, 1);
    assert_eq!(chat.active_hold_remaining, 1);
    Ok(())
}

#[tokio::test]
async fn streaming_responses_terminal_id_continues_episode_across_restart() -> anyhow::Result<()> {
    let harness = HttpHarness::streaming_responses().await?;
    let first_app = harness.assemble().await?;
    let first_server = server(&first_app);
    let owner = "streaming-responses-owner";
    let bearer = add_owner(&first_app.db, owner).await?;

    let first_id =
        post_streaming_responses(&first_server, &bearer, "inspect the repository", None).await?;
    assert!(first_id.starts_with("brc_"));
    assert!(!first_id.contains("provider-only-response-"));
    let second_id = post_streaming_responses(
        &first_server,
        &bearer,
        "continue the implementation",
        Some(&first_id),
    )
    .await?;
    assert_ne!(first_id, second_id);

    let episode_ids = owner_episode_ids(&first_app.db, owner).await?;
    assert_eq!(episode_ids.len(), 1, "both turns must share one episode");
    let diagnostics = request_diagnostics(&first_app.db, owner).await?;
    assert_eq!(diagnostics.len(), 2);
    assert!(
        diagnostics
            .iter()
            .all(|(protocol, _)| protocol == "responses")
    );
    assert_eq!(diagnostics[0].1, None);
    assert!(
        diagnostics[1]
            .1
            .as_deref()
            .is_some_and(|parent| parent.starts_with("trajectory-request-")),
        "second turn must persist an opaque native parent"
    );
    let before_restart = normalized_outcome(&first_app.db, owner).await?;
    assert_eq!(
        before_restart.correlation_sources,
        ["explicit_root", "native_parent_id"]
    );
    assert_eq!(before_restart.completeness, HistoryCompleteness::Complete);
    assert_eq!(before_restart.request_count, 2);
    assert_eq!(before_restart.settled_request_count, 2);
    let published = first_app
        .trajectory_outbox_publisher
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("trajectory publisher missing"))?
        .drain_after_active_worker()
        .await?;
    assert_eq!(published.failed, 0);
    for surface in trajectory_durable_surfaces(&first_app.db, owner).await? {
        assert!(
            !surface.contains("provider-only-response-"),
            "raw upstream response id escaped into trajectory storage"
        );
    }

    drop(first_server);
    drop(first_app);

    let restarted_app = harness.assemble().await?;
    let restarted_server = server(&restarted_app);
    let third_id = post_streaming_responses(
        &restarted_server,
        &bearer,
        "review the completed change",
        Some(&second_id),
    )
    .await?;
    assert_ne!(second_id, third_id);
    let (forwarded_parents, served_models) = {
        let responses_state = harness
            .responses_state
            .as_ref()
            .expect("streaming harness has Responses oracle")
            .lock()
            .expect("Responses mock state poisoned");
        (
            responses_state.forwarded_parents.clone(),
            responses_state.served_models.clone(),
        )
    };
    assert_eq!(
        forwarded_parents,
        [
            None,
            Some("provider-only-response-0".to_owned()),
            Some("provider-only-response-1".to_owned())
        ],
        "the assembled app must forward exactly the provider-issued continuation chain"
    );
    assert_eq!(
        owner_episode_ids(&restarted_app.db, owner).await?,
        episode_ids
    );
    let after_restart = normalized_outcome(&restarted_app.db, owner).await?;
    let selected_models = after_restart
        .selected_tiers
        .iter()
        .map(|tier| format!("{tier}-model"))
        .collect::<Vec<_>>();
    assert_eq!(
        served_models, selected_models,
        "actual serving models must equal the signed policy decisions"
    );
    assert_eq!(
        after_restart.correlation_sources,
        ["explicit_root", "native_parent_id", "native_parent_id"]
    );
    assert_eq!(after_restart.completeness, HistoryCompleteness::Complete);
    assert_eq!(after_restart.request_count, 3);
    assert_eq!(after_restart.settled_request_count, 3);
    let published = restarted_app
        .trajectory_outbox_publisher
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("restarted trajectory publisher missing"))?
        .drain_after_active_worker()
        .await?;
    assert_eq!(published.failed, 0);
    for surface in trajectory_durable_surfaces(&restarted_app.db, owner).await? {
        assert!(
            !surface.contains("provider-only-response-"),
            "raw upstream response id escaped after restart"
        );
    }
    for surface in continuation_durable_surfaces(&restarted_app.db).await? {
        assert!(
            !surface.contains("provider-only-response-"),
            "raw upstream response id escaped continuation encryption"
        );
    }
    Ok(())
}

#[tokio::test]
async fn streaming_responses_continuation_is_independent_of_trajectory() -> anyhow::Result<()> {
    let harness = HttpHarness::streaming_responses_with_trajectory(false).await?;
    let app = harness.assemble().await?;
    let server = server(&app);
    let owner = "streaming-responses-no-trajectory";
    let bearer = add_owner(&app.db, owner).await?;

    let first_id =
        post_streaming_responses(&server, &bearer, "inspect the repository", None).await?;
    let second_id = post_streaming_responses(&server, &bearer, "continue", Some(&first_id)).await?;
    assert!(first_id.starts_with("brc_"));
    assert!(second_id.starts_with("brc_"));
    assert_ne!(first_id, second_id);
    assert!(owner_episode_ids(&app.db, owner).await?.is_empty());
    assert_eq!(
        harness
            .responses_state
            .as_ref()
            .expect("streaming harness has Responses oracle")
            .lock()
            .expect("Responses mock state poisoned")
            .forwarded_parents,
        [None, Some("provider-only-response-0".to_owned())]
    );
    Ok(())
}

#[tokio::test]
async fn invalid_responses_request_id_has_no_upstream_or_durable_side_effects() -> anyhow::Result<()>
{
    let harness = HttpHarness::streaming_responses().await?;
    let app = harness.assemble().await?;
    let server = server(&app);
    let bearer = add_owner(&app.db, "invalid-responses-request-id").await?;

    for stream in [false, true] {
        let response = server
            .post(InboundProtocol::Responses.endpoint())
            .add_header("authorization", format!("Bearer {bearer}"))
            .add_header("x-bitrouter-request-id", "x".repeat(129))
            .json(&json!({"model": "@auto", "input": "ping", "stream": stream}))
            .await;
        assert_eq!(response.status_code().as_u16(), 400);
    }

    assert!(
        harness
            .strong
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
    assert!(
        harness
            .economy
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
    assert_eq!(table_row_count(&app.db, "trajectory_requests").await?, 0);
    assert_eq!(table_row_count(&app.db, "provider_continuations").await?, 0);
    Ok(())
}

#[tokio::test]
async fn guarded_continuation_fails_before_upstream_when_authority_changes() -> anyhow::Result<()> {
    let harness = HttpHarness::streaming_responses_split_authority().await?;
    let app = harness.assemble().await?;
    let server = server(&app);
    let owner = "split-authority-continuation";
    let bearer = add_owner(&app.db, owner).await?;
    let first_id =
        post_streaming_responses(&server, &bearer, "inspect the repository", None).await?;
    let second_id = post_streaming_responses(
        &server,
        &bearer,
        "continue the implementation",
        Some(&first_id),
    )
    .await?;

    let response = server
        .post(InboundProtocol::Responses.endpoint())
        .add_header("authorization", format!("Bearer {bearer}"))
        .json(&json!({
            "model": "@auto",
            "stream": true,
            "input": "review the completed change",
            "previous_response_id": second_id
        }))
        .await;
    assert_eq!(response.status_code().as_u16(), 400);
    assert!(response.text().contains("target is unavailable or changed"));
    assert_eq!(
        harness
            .economy
            .received_requests()
            .await
            .unwrap_or_default()
            .len(),
        2
    );
    assert!(
        harness
            .strong
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "the changed authority must fail before any strong-provider dispatch"
    );
    assert_eq!(table_row_count(&app.db, "provider_continuations").await?, 2);
    let statuses = trajectory_request_statuses(&app.db, owner).await?;
    assert_eq!(
        statuses.iter().filter(|status| *status == "failed").count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| *status == "settled")
            .count(),
        2
    );
    let events = events_for_only_episode(&app.db, owner).await?;
    let failed = events
        .iter()
        .find(|event| {
            event.kind == TrajectoryEventKind::RequestSettled
                && event.evidence.categorical.get("settlement.outcome")
                    == Some(&"failed".to_owned())
        })
        .ok_or_else(|| anyhow::anyhow!("changed-authority request was not settled as failed"))?;
    assert!(
        !failed
            .evidence
            .categorical
            .contains_key("settlement.provider")
    );
    assert!(!failed.evidence.categorical.contains_key("settlement.model"));
    Ok(())
}

#[tokio::test]
async fn synthetic_recovery_hold_and_streak_bound_are_task_neutral() -> anyhow::Result<()> {
    let requests = fixture_requests(include_str!(
        "fixtures/trajectory/recovery_then_repeat.jsonl"
    ))?;

    let disabled = HttpHarness::new(false).await?;
    let disabled_app = disabled.assemble().await?;
    let disabled_server = server(&disabled_app);
    let disabled_bearer = add_owner(&disabled_app.db, "guard-disabled").await?;
    for request in &requests {
        post_fixture(
            &disabled_server,
            InboundProtocol::Chat,
            &disabled_bearer,
            request,
            None,
        )
        .await?;
    }
    assert!(
        owner_episode_ids(&disabled_app.db, "guard-disabled")
            .await?
            .is_empty(),
        "an existing lock without progress_guard must not start persistence"
    );
    assert_eq!(
        disabled
            .strong
            .received_requests()
            .await
            .unwrap_or_default()
            .len(),
        0
    );
    assert_eq!(
        disabled
            .economy
            .received_requests()
            .await
            .unwrap_or_default()
            .len(),
        requests.len()
    );

    let enabled = HttpHarness::new(true).await?;
    let enabled_app = enabled.assemble().await?;
    let enabled_server = server(&enabled_app);
    let enabled_bearer = add_owner(&enabled_app.db, "guard-enabled").await?;
    for request in &requests {
        post_fixture(
            &enabled_server,
            InboundProtocol::Chat,
            &enabled_bearer,
            request,
            None,
        )
        .await?;
    }
    let outcome = normalized_outcome(&enabled_app.db, "guard-enabled").await?;
    assert_eq!(
        outcome.selected_tiers,
        [
            "economy", "strong", "strong", "strong", "economy", "economy", "strong"
        ]
    );
    assert_eq!(outcome.recovery_count, 1);

    let events = events_for_only_episode(&enabled_app.db, "guard-enabled").await?;
    let mut terminal_streaks = Vec::new();
    for (index, event) in events.iter().enumerate() {
        if event.kind == TrajectoryEventKind::RequestSettled {
            let snapshot = reduce(&events[..=index], &BTreeSet::from(["strong".to_owned()]))?;
            terminal_streaks.push(snapshot.health.consecutive_unprotected_requests);
        }
    }
    assert_eq!(terminal_streaks, [1, 0, 0, 0, 1, 2, 0]);
    assert!(terminal_streaks.into_iter().all(|streak| streak <= 3));
    assert_eq!(
        enabled
            .strong
            .received_requests()
            .await
            .unwrap_or_default()
            .len(),
        4
    );
    assert_eq!(
        enabled
            .economy
            .received_requests()
            .await
            .unwrap_or_default()
            .len(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn task_model_tool_and_harness_labels_are_noncausal() -> anyhow::Result<()> {
    let harness = HttpHarness::new(true).await?;
    let assembled = harness.assemble().await?;
    let server = server(&assembled);
    let baseline_bearer = add_owner(&assembled.db, "independence-baseline").await?;
    let mutated_bearer = add_owner(&assembled.db, "independence-mutated").await?;
    let requests = fixture_requests(include_str!(
        "fixtures/trajectory/recovery_then_repeat.jsonl"
    ))?;

    for request in &requests {
        post_fixture(
            &server,
            InboundProtocol::Chat,
            &baseline_bearer,
            request,
            None,
        )
        .await?;

        let mut mutated = request.body.clone();
        replace_fixture_value(
            &mut mutated,
            &[
                ("neutral task alpha", "changed task omega"),
                ("inspect", "observe"),
                ("@auto", "@flex"),
            ],
        );
        post_body(
            &server,
            InboundProtocol::Chat,
            &mutated_bearer,
            &request.stage,
            &mutated,
            &[
                ("user-agent", "generic-harness-beta"),
                ("x-bitrouter-harness", "codex"),
                ("x-bitrouter-workflow-name", "workflow-beta"),
                ("x-bitrouter-case-id", "case-beta"),
                ("x-bitrouter-benchmark-id", "benchmark-beta"),
                ("x-bitrouter-trial-id", "trial-beta"),
            ],
        )
        .await?;
    }

    let baseline = normalized_outcome(&assembled.db, "independence-baseline").await?;
    let mutated = normalized_outcome(&assembled.db, "independence-mutated").await?;
    assert_eq!(baseline.progress_view(), mutated.progress_view());
    assert_eq!(baseline.correlation_sources, mutated.correlation_sources);

    let baseline_routes: Vec<_> = events_for_only_episode(&assembled.db, "independence-baseline")
        .await?
        .into_iter()
        .filter(|event| event.kind == TrajectoryEventKind::RouteIntentRecorded)
        .map(|event| (event.evidence.structural, event.evidence.categorical))
        .collect();
    let mutated_routes: Vec<_> = events_for_only_episode(&assembled.db, "independence-mutated")
        .await?
        .into_iter()
        .filter(|event| event.kind == TrajectoryEventKind::RouteIntentRecorded)
        .map(|event| (event.evidence.structural, event.evidence.categorical))
        .collect();
    assert_eq!(baseline_routes, mutated_routes);
    Ok(())
}

#[tokio::test]
async fn incomplete_conflicting_interleaved_and_owner_histories_are_explicit() -> anyhow::Result<()>
{
    let harness = HttpHarness::new(true).await?;
    let assembled = harness.assemble().await?;
    let server = server(&assembled);
    let complete = fixture_requests(InboundProtocol::Chat.fixture())?;

    // Identical content under distinct authenticated owners never shares state.
    for owner in ["isolation-a", "isolation-b"] {
        let bearer = add_owner(&assembled.db, owner).await?;
        post_fixture(&server, InboundProtocol::Chat, &bearer, &complete[0], None).await?;
        assert_eq!(
            normalized_outcome(&assembled.db, owner).await?.completeness,
            HistoryCompleteness::Complete
        );
    }
    assert_ne!(
        owner_episode_ids(&assembled.db, "isolation-a").await?,
        owner_episode_ids(&assembled.db, "isolation-b").await?
    );

    // One owner can interleave two distinct conversations without merging them.
    let interleaved = "interleaved-owner";
    let bearer = add_owner(&assembled.db, interleaved).await?;
    let mut root_b = complete[0].body.clone();
    let mut child_b = complete[1].body.clone();
    for body in [&mut root_b, &mut child_b] {
        replace_fixture_value(body, &[("neutral task alpha", "neutral task bravo")]);
    }
    post_fixture(&server, InboundProtocol::Chat, &bearer, &complete[0], None).await?;
    post_body(
        &server,
        InboundProtocol::Chat,
        &bearer,
        "root-b",
        &root_b,
        &[],
    )
    .await?;
    post_fixture(&server, InboundProtocol::Chat, &bearer, &complete[1], None).await?;
    post_body(
        &server,
        InboundProtocol::Chat,
        &bearer,
        "child-b",
        &child_b,
        &[],
    )
    .await?;
    let interleaved_episodes = owner_episode_ids(&assembled.db, interleaved).await?;
    assert_eq!(interleaved_episodes.len(), 2);
    for episode in interleaved_episodes {
        let stored = TrajectoryStore::new(assembled.db.clone())
            .episode(interleaved, &episode)
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing interleaved episode"))?;
        assert_eq!(stored.completeness, HistoryCompleteness::Complete);
    }

    // Two indistinguishable roots make the later prefix explicitly ambiguous.
    let conflict_owner = "prefix-conflict";
    let conflict_bearer = add_owner(&assembled.db, conflict_owner).await?;
    for _ in 0..2 {
        post_fixture(
            &server,
            InboundProtocol::Chat,
            &conflict_bearer,
            &complete[0],
            None,
        )
        .await?;
    }
    post_fixture(
        &server,
        InboundProtocol::Chat,
        &conflict_bearer,
        &complete[1],
        None,
    )
    .await?;
    let conflict_episodes = owner_episode_ids(&assembled.db, conflict_owner).await?;
    assert_eq!(conflict_episodes.len(), 3);
    let mut incomplete = 0;
    for episode in conflict_episodes {
        let stored = TrajectoryStore::new(assembled.db.clone())
            .episode(conflict_owner, &episode)
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing conflict episode"))?;
        incomplete += usize::from(stored.completeness == HistoryCompleteness::Incomplete);
    }
    assert_eq!(incomplete, 1);

    // Truncation and compaction retain no guess at missing ancestry.
    for (owner, body) in [
        ("truncated-owner", complete[1].body.clone()),
        (
            "compacted-owner",
            json!({
                "model": "@auto",
                "messages": [
                    {"role": "assistant", "content": "bounded structural summary"},
                    {"role": "user", "content": "continue after compaction"}
                ]
            }),
        ),
    ] {
        let bearer = add_owner(&assembled.db, owner).await?;
        post_body(&server, InboundProtocol::Chat, &bearer, owner, &body, &[]).await?;
        assert_eq!(
            normalized_outcome(&assembled.db, owner).await?.completeness,
            HistoryCompleteness::Incomplete
        );
    }

    // Unknown and cross-owner native parents fail closed and remain opaque.
    let parent_owner = "parent-owner";
    let parent_bearer = add_owner(&assembled.db, parent_owner).await?;
    let parent_id = post_body(
        &server,
        InboundProtocol::Responses,
        &parent_bearer,
        "parent-root",
        &json!({"model": "@auto", "input": "root"}),
        &[],
    )
    .await?;
    for (owner, raw_parent) in [
        ("unknown-parent-owner", "response-does-not-exist"),
        ("foreign-parent-owner", parent_id.as_str()),
    ] {
        let bearer = add_owner(&assembled.db, owner).await?;
        let response = server
            .post(InboundProtocol::Responses.endpoint())
            .add_header("authorization", format!("Bearer {bearer}"))
            .json(&json!({
                "model": "@auto",
                "previous_response_id": raw_parent,
                "input": "independent root"
            }))
            .await;
        assert_eq!(response.status_code().as_u16(), 400);
        let events = events_for_only_episode(&assembled.db, owner).await?;
        assert_eq!(
            events[0].evidence.categorical.get("history.completeness"),
            Some(&"incomplete".to_owned())
        );
        let digest = events[0]
            .evidence
            .digests
            .get("correlation.native_parent")
            .ok_or_else(|| anyhow::anyhow!("missing native parent digest"))?;
        assert!(digest.starts_with("hmac-sha256:"));
        assert!(!digest.contains(raw_parent));
        assert_eq!(request_diagnostics(&assembled.db, owner).await?[0].1, None);
    }
    assert_ne!(
        owner_episode_ids(&assembled.db, parent_owner).await?,
        owner_episode_ids(&assembled.db, "foreign-parent-owner").await?
    );
    Ok(())
}

#[tokio::test]
async fn file_database_restart_preserves_episode_and_hold() -> anyhow::Result<()> {
    let harness = HttpHarness::new(true).await?;
    let first_app = harness.assemble().await?;
    let first_server = server(&first_app);
    let bearer = add_owner(&first_app.db, "restart-owner").await?;
    let requests = fixture_requests(InboundProtocol::Chat.fixture())?;
    for request in &requests[..2] {
        post_fixture(&first_server, InboundProtocol::Chat, &bearer, request, None).await?;
    }
    let before = normalized_outcome(&first_app.db, "restart-owner").await?;
    assert_eq!(before.selected_tiers, ["economy", "strong"]);
    assert_eq!(before.active_hold_remaining, 2);
    drop(first_server);
    drop(first_app);

    let restarted_app = harness.assemble().await?;
    let restarted_server = server(&restarted_app);
    post_fixture(
        &restarted_server,
        InboundProtocol::Chat,
        &bearer,
        &requests[2],
        None,
    )
    .await?;
    let after = normalized_outcome(&restarted_app.db, "restart-owner").await?;
    assert_eq!(after.selected_tiers, ["economy", "strong", "strong"]);
    assert_eq!(after.active_hold_remaining, 1);
    assert_eq!(after.recovery_count, 1);
    Ok(())
}

#[tokio::test]
async fn transport_retry_identity_is_idempotent_without_becoming_a_route_key() -> anyhow::Result<()>
{
    let harness = HttpHarness::new(true).await?;
    let assembled = harness.assemble().await?;
    let server = server(&assembled);
    let bearer = add_owner(&assembled.db, "retry-owner").await?;
    let opening = fixture_requests(InboundProtocol::Chat.fixture())?.remove(0);

    let first = post_body(
        &server,
        InboundProtocol::Chat,
        &bearer,
        "retry-first",
        &opening.body,
        &[("x-bitrouter-request-id", "transport-retry-001")],
    )
    .await?;
    let second = post_body(
        &server,
        InboundProtocol::Chat,
        &bearer,
        "retry-second",
        &opening.body,
        &[("x-bitrouter-request-id", "transport-retry-001")],
    )
    .await?;

    assert_eq!(first, second);
    let outcome = normalized_outcome(&assembled.db, "retry-owner").await?;
    assert_eq!(outcome.request_count, 1);
    assert_eq!(outcome.settled_request_count, 1);
    assert_eq!(outcome.selected_tiers, ["economy"]);
    Ok(())
}

#[tokio::test]
async fn auto_template_recovery_at_strong_activates_hold_for_next_normal_route()
-> anyhow::Result<()> {
    let mut lock: PolicyLock = serde_saphyr::from_str(include_str!(
        "../../../templates/auto-router/policy-lock.yaml"
    ))?;
    let auto = lock
        .policies
        .get_mut("auto")
        .ok_or_else(|| anyhow::anyhow!("template lock is missing the auto policy"))?;
    auto.tiers
        .insert("strong".into(), "strong:strong-model".into());
    auto.tiers
        .insert("balanced".into(), "balanced:balanced-model".into());
    auto.tiers
        .insert("economy".into(), "economy:economy-model".into());

    let harness = HttpHarness::with_lock(lock).await?;
    let assembled = harness.assemble().await?;
    let server = server(&assembled);
    let bearer = add_owner(&assembled.db, "template-hold-owner").await?;
    let requests = fixture_requests(InboundProtocol::Chat.fixture())?;
    for request in &requests {
        post_fixture(&server, InboundProtocol::Chat, &bearer, request, None).await?;
    }

    let outcome = normalized_outcome(&assembled.db, "template-hold-owner").await?;
    assert_eq!(
        outcome.selected_tiers,
        ["strong", "strong", "strong"],
        "the template recovery is already statically strong but must activate hold for the next economy route"
    );
    assert_eq!(outcome.recovery_count, 1);
    assert_eq!(outcome.active_hold_remaining, 1);
    let events = events_for_only_episode(&assembled.db, "template-hold-owner").await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == TrajectoryEventKind::GuardActivated)
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn recovery_at_another_protected_tier_activates_hold_for_unprotected_followup()
-> anyhow::Result<()> {
    let mut lock: PolicyLock = serde_saphyr::from_str(include_str!(
        "../../../templates/auto-router/policy-lock.yaml"
    ))?;
    let auto = lock
        .policies
        .get_mut("auto")
        .ok_or_else(|| anyhow::anyhow!("template lock is missing the auto policy"))?;
    auto.tiers
        .insert("strong".into(), "strong:strong-model".into());
    auto.tiers
        .insert("balanced".into(), "balanced:balanced-model".into());
    auto.tiers
        .insert("economy".into(), "economy:economy-model".into());
    auto.default_tier = Some("balanced".into());
    auto.progress_guard
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("template auto policy has no progress guard"))?
        .protected_tiers
        .insert("balanced".into());

    let harness = HttpHarness::with_lock(lock).await?;
    let assembled = harness.assemble().await?;
    let server = server(&assembled);
    let owner = "alternate-protected-owner";
    let bearer = add_owner(&assembled.db, owner).await?;
    let requests = fixture_requests(InboundProtocol::Chat.fixture())?;
    for request in &requests {
        post_fixture(&server, InboundProtocol::Chat, &bearer, request, None).await?;
    }

    let outcome = normalized_outcome(&assembled.db, owner).await?;
    assert_eq!(
        outcome.selected_tiers,
        ["balanced", "balanced", "strong"],
        "recovery preserves its protected candidate, then hold escalates the next unprotected candidate"
    );
    assert_eq!(outcome.active_hold_remaining, 1);
    let events = events_for_only_episode(&assembled.db, owner).await?;
    let candidate_tiers = events
        .iter()
        .filter(|event| event.kind == TrajectoryEventKind::RouteIntentRecorded)
        .filter_map(|event| event.evidence.categorical.get("route.candidate_tier"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(candidate_tiers, ["balanced", "balanced", "economy"]);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == TrajectoryEventKind::GuardActivated)
            .count(),
        1
    );
    let episode_id = owner_episode_ids(&assembled.db, owner)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("alternate protected episode is missing"))?;
    match TrajectoryStore::new(assembled.db.clone())
        .audit_episode(owner, &episode_id)
        .await?
    {
        EpisodeAudit::Valid { snapshot, .. } => {
            assert_eq!(snapshot.active_hold_remaining, 1);
        }
        EpisodeAudit::Corrupt { reason, .. } => {
            anyhow::bail!("alternate protected episode failed audit: {reason}")
        }
    }
    Ok(())
}

#[test]
fn auto_template_explicitly_opts_into_conservative_progress_control() -> anyhow::Result<()> {
    let config = config::parse_with(
        include_str!("../../../templates/auto-router/bitrouter.yaml"),
        |_| None,
    )?;
    assert!(
        config.trajectory.enabled,
        "the generalized @auto template must explicitly enable trajectory persistence"
    );

    let lock: PolicyLock = serde_saphyr::from_str(include_str!(
        "../../../templates/auto-router/policy-lock.yaml"
    ))?;
    bitrouter::policy_lock::validate_for_config(&config, &lock)?;
    let auto = lock
        .policies
        .get("auto")
        .ok_or_else(|| anyhow::anyhow!("template lock is missing the auto policy"))?;
    let guard = auto.progress_guard.as_ref().ok_or_else(|| {
        anyhow::anyhow!("the generalized @auto policy must explicitly define progress_guard")
    })?;
    assert_eq!(guard.escalation_tier, "strong");
    assert!(guard.protected_tiers.contains("strong"));
    assert_eq!(guard.max_recovery_count, Some(1));
    assert_eq!(guard.max_consecutive_unprotected, Some(3));
    assert_eq!(guard.max_same_projection_unprotected, Some(3));
    assert_eq!(guard.max_episode_requests, Some(24));
    assert_eq!(guard.max_episode_elapsed_ms, Some(1_800_000));
    assert_eq!(guard.max_episode_cost_micro_usd, None);
    assert_eq!(guard.hold_for_requests, 2);
    assert_eq!(guard.incomplete_history, IncompleteHistoryAction::Escalate);
    Ok(())
}
