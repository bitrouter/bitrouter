//! Native Responses continuation coverage for named policy routers.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum_test::TestServer;
use bitrouter::config;
use bitrouter::gateway::http::AppState;
use bitrouter::gateway::http::build_router;
use bitrouter::metering::entities::requests;
use sea_orm::EntityTrait;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[derive(Default)]
struct NativeResponsesState {
    next_id: usize,
    issued_ids: BTreeSet<String>,
    forwarded_parents: Vec<Option<String>>,
    served_models: Vec<String>,
    forwarded_bodies: Vec<Value>,
}

struct NativeResponsesResponder {
    state: Arc<Mutex<NativeResponsesState>>,
}

impl Respond for NativeResponsesResponder {
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
        let model = match body.get("model").and_then(Value::as_str) {
            Some(model) => model.to_owned(),
            None => return ResponseTemplate::new(400).set_body_string("missing model"),
        };
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return ResponseTemplate::new(500).set_body_string("state lock poisoned"),
        };
        state.forwarded_parents.push(previous_response_id.clone());
        state.served_models.push(model.clone());
        state.forwarded_bodies.push(body.clone());
        if previous_response_id
            .as_ref()
            .is_some_and(|parent| !state.issued_ids.contains(parent))
        {
            return ResponseTemplate::new(409).set_body_json(json!({
                "error": {
                    "type": "invalid_request_error",
                    "message": "previous_response_id was not issued by this provider"
                }
            }));
        }
        let response_id = format!("provider-response-{}", state.next_id);
        state.next_id += 1;
        state.issued_ids.insert(response_id.clone());
        drop(state);

        let item = json!({
            "id": "provider-item", "type": "message", "status": "completed", "role": "assistant",
            "content": [{"type": "output_text", "text": "ok"}],
        });
        let response = json!({
            "id": response_id, "object": "response", "status": "completed", "model": model,
            "output": [item], "usage": {"input_tokens":11,"output_tokens":7,"total_tokens":18},
        });
        if body["stream"].as_bool() != Some(true) {
            return ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(response);
        }
        let events = [
            json!({"type":"response.created","sequence_number":0,"response":{
                "id":response_id,"object":"response","status":"in_progress","model":model,"output":[]}}),
            json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,
                "item":{"id":"provider-item","type":"message","status":"in_progress","role":"assistant","content":[]}}),
            json!({"type":"response.content_part.added","sequence_number":2,"item_id":"provider-item",
                "output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}),
            json!({"type":"response.output_text.delta","sequence_number":3,"item_id":"provider-item",
                "output_index":0,"content_index":0,"delta":"ok"}),
            json!({"type":"response.output_text.done","sequence_number":4,"item_id":"provider-item",
                "output_index":0,"content_index":0,"text":"ok"}),
            json!({"type":"response.content_part.done","sequence_number":5,"item_id":"provider-item",
                "output_index":0,"content_index":0,"part":{"type":"output_text","text":"ok"}}),
            json!({"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":item}),
            json!({"type":"response.completed","sequence_number":7,"response":response}),
        ];
        let body = events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().map_or("message", |kind| kind)
                )
            })
            .collect::<String>();
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }
}

fn server(assembled: &bitrouter::Assembled) -> anyhow::Result<TestServer> {
    let language_model = assembled
        .app
        .language_model()
        .context("assembled app has no language-model pipeline")?
        .clone();
    Ok(TestServer::new(build_router(AppState {
        language_model,
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    })))
}

async fn post_response(
    server: &TestServer,
    previous_response_id: Option<&str>,
) -> anyhow::Result<String> {
    let mut body = json!({
        "model": "bitrouter/coding",
        "input": "continue the coding task",
        "stream": false
    });
    if let Some(previous_response_id) = previous_response_id {
        body["previous_response_id"] = Value::String(previous_response_id.to_owned());
    }
    let response = server.post("/v1/responses").json(&body).await;
    let status = response.status_code();
    let response_body = response.text();
    assert_eq!(
        status.as_u16(),
        200,
        "Responses request failed: {response_body}"
    );
    let response: Value = serde_json::from_str(&response_body)?;
    assert_eq!(response["model"], "selected-model");
    response["id"]
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Responses result has no continuation id: {response}"))
}

#[tokio::test]
async fn named_router_identity_survives_native_responses_continuation() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    let upstream_state = Arc::new(Mutex::new(NativeResponsesState::default()));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(NativeResponsesResponder {
            state: upstream_state.clone(),
        })
        .mount(&upstream)
        .await;

    let home = tempfile::tempdir()?;
    let config_path = home.path().join("bitrouter.yaml");
    let policy_path = home.path().join("policy.yaml");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        home.path().join("requests.db").display()
    );
    let config_text = format!(
        r#"inherit_defaults: false
server:
  skip_auth: true
database:
  url: {}
policy:
  path: ./policy.yaml
providers:
  responses:
    api_base: {}
    api_key: test-key
    api_protocol:
      - "*": responses
    models:
      - id: base-model
      - id: selected-model
routers:
  coding:
    selection:
      kind: policy
      policy: coding
      base_model: responses:base-model
"#,
        serde_json::to_string(&database_url)?,
        serde_json::to_string(&upstream.uri())?,
    );
    let policy_text = r#"spec_version: policy/1.0.0-beta.1
policies:
  coding:
    tiers:
      strong: {model: responses:selected-model}
    default_tier: strong
    tool_use_tier: strong
    tool_safe_tiers: [strong]
"#;
    tokio::fs::write(&config_path, &config_text).await?;
    let source: bitrouter_ai::policy::Document = serde_saphyr::from_str(policy_text)?;
    tokio::fs::write(
        &policy_path,
        bitrouter::policy_source::deterministic_yaml(&source)?,
    )
    .await?;

    let config = config::parse(&config_text)?;
    let assembled = bitrouter::build_app_with_path(&config, Some(&config_path)).await?;
    let server = server(&assembled)?;

    let first_id = post_response(&server, None).await?;
    assert!(first_id.starts_with("brc_"));
    assert!(!first_id.contains("provider-response"));
    let second_id = post_response(&server, Some(&first_id)).await?;
    assert!(second_id.starts_with("brc_"));
    assert_ne!(first_id, second_id);

    assembled
        .app
        .language_model()
        .context("assembled app has no language-model pipeline")?
        .drain_required_pending_settlements()
        .await?;

    let (forwarded_parents, served_models) = {
        let state = upstream_state
            .lock()
            .map_err(|_| anyhow::anyhow!("native Responses state lock poisoned"))?;
        (state.forwarded_parents.clone(), state.served_models.clone())
    };
    assert_eq!(
        forwarded_parents,
        [None, Some("provider-response-0".to_owned())]
    );
    assert_eq!(served_models, ["selected-model", "selected-model"]);

    let rows = requests::Entity::find().all(&assembled.db).await?;
    assert_eq!(rows.len(), 2, "each continuation turn must settle once");
    let mut binding_digests = BTreeSet::new();
    for row in rows {
        assert_eq!(row.router_id.as_deref(), Some("coding"));
        assert_eq!(row.original_selector.as_deref(), Some("bitrouter/coding"));
        assert_eq!(row.provider_id, "responses");
        assert_eq!(row.model_id, "selected-model");
        binding_digests.insert(
            row.binding_digest
                .context("settled router request has no binding digest")?,
        );
    }
    assert_eq!(binding_digests.len(), 1);
    assert!(
        binding_digests
            .iter()
            .all(|digest| digest.starts_with("router-v1:sha256:"))
    );
    Ok(())
}

/// Current-source fixture replacing continuation cases formerly embedded in the
/// guard suite. Source reloads provide explicit model changes; no guard, learning
/// state or trajectory activation is involved.
struct SourceContinuationFixture {
    home: tempfile::TempDir,
    config: config::Config,
    config_path: std::path::PathBuf,
    original: MockServer,
    alternate: MockServer,
    state: Arc<Mutex<NativeResponsesState>>,
}

impl SourceContinuationFixture {
    async fn new() -> anyhow::Result<Self> {
        let home = tempfile::tempdir()?;
        let config_path = home.path().join("bitrouter.yaml");
        let state = Arc::new(Mutex::new(NativeResponsesState::default()));
        let original = MockServer::start().await;
        let alternate = MockServer::start().await;
        for upstream in [&original, &alternate] {
            Mock::given(method("POST"))
                .and(path("/responses"))
                .respond_with(NativeResponsesResponder {
                    state: state.clone(),
                })
                .mount(upstream)
                .await;
        }
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            home.path().join("requests.db").display()
        );
        let raw = format!(
            r#"inherit_defaults: false
server:
  skip_auth: true
database:
  url: {}
policy:
  path: ./policy.yaml
providers:
  original:
    api_base: {}
    api_key: original-fixture-key
    api_protocol:
      - '*': responses
    models:
      - id: selected-model
      - id: replacement-model
  alternate:
    api_base: {}
    api_key: alternate-fixture-key
    api_protocol:
      - '*': responses
    models:
      - id: cheap-model
routers:
  coding:
    selection:
      kind: policy
      policy: coding
      base_model: original:selected-model
"#,
            serde_json::to_string(&database_url)?,
            serde_json::to_string(&original.uri())?,
            serde_json::to_string(&alternate.uri())?
        );
        std::fs::write(&config_path, &raw)?;
        let fixture = Self {
            home,
            config: config::parse_with(&raw, |_| None)?,
            config_path,
            original,
            alternate,
            state,
        };
        fixture.write_source("strong", false)?;
        Ok(fixture)
    }

    fn write_source(&self, default_tier: &str, remove_original: bool) -> anyhow::Result<()> {
        use bitrouter_ai::policy::{Document, Policy, SpecVersion, Target};
        use std::collections::BTreeMap;
        let source = Document {
            spec_version: SpecVersion::Beta1,
            policies: BTreeMap::from([(
                "coding".into(),
                Policy {
                    tiers: BTreeMap::from([
                        (
                            "strong".into(),
                            Target {
                                model: if remove_original {
                                    "original:replacement-model".into()
                                } else {
                                    "original:selected-model".into()
                                },
                                effort: None,
                            },
                        ),
                        (
                            "cheap".into(),
                            Target {
                                model: "alternate:cheap-model".into(),
                                effort: None,
                            },
                        ),
                    ]),
                    default_tier: default_tier.into(),
                    routes: BTreeMap::new(),
                    classifier_digest: None,
                    tool_use_tier: Some("strong".into()),
                    tool_safe_tiers: vec!["strong".into(), "cheap".into()],
                },
            )]),
        };
        let path = self.home.path().join("policy.yaml");
        let expected = if path.exists() {
            let current: Document = serde_saphyr::from_str(&std::fs::read_to_string(&path)?)?;
            Some(bitrouter::policy_source::semantic_digest(&current)?)
        } else {
            None
        };
        bitrouter::policy_source::write_atomic(&path, expected.as_deref(), &source)?;
        Ok(())
    }

    async fn assemble(&self) -> anyhow::Result<bitrouter::Assembled> {
        bitrouter::build_app_with_path(&self.config, Some(&self.config_path)).await
    }

    async fn reload(
        &self,
        assembled: &bitrouter::Assembled,
        remove_original: bool,
    ) -> anyhow::Result<()> {
        self.write_source("cheap", remove_original)?;
        assembled
            .policy_runtime
            .reload_for_config(&self.config, Some(&self.config_path))
            .await
    }
}

async fn continuation_reply(server: &TestServer, body: &Value) -> anyhow::Result<Value> {
    let response = server.post("/v1/responses").json(body).await;
    assert_eq!(
        response.status_code().as_u16(),
        200,
        "Responses failed: {}",
        response.text()
    );
    if body["stream"].as_bool() != Some(true) {
        return Ok(response.json());
    }
    let text = response.text();
    let events = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let terminal = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .context("stream omitted successful terminal")?;
    let id = terminal["response"]["id"]
        .as_str()
        .context("terminal omitted continuation ID")?;
    let lifecycle_ids = events
        .iter()
        .filter_map(|event| event["response"]["id"].as_str())
        .collect::<Vec<_>>();
    assert!(!lifecycle_ids.is_empty());
    assert!(
        lifecycle_ids.iter().all(|value| *value == id),
        "lifecycle ID changed: {events:?}"
    );
    Ok(terminal["response"].clone())
}

fn visible_history(root: &str, assistant: &str) -> Value {
    json!([
        {"type":"message","role":"user","content":[{"type":"input_text","text":root}]},
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":assistant}]},
        {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]},
    ])
}

#[tokio::test]
async fn current_source_continuation_pins_hidden_or_uncommitted_history_and_detaches_exact_history()
-> anyhow::Result<()> {
    for stream in [false, true] {
        // Exact visible history may detach; suffix-only, unrelated and tampered
        // history must keep the original provider/credential/model authority.
        for (input, detached) in [
            (json!("continue"), false),
            (visible_history("root", "ok"), true),
            (visible_history("unrelated root", "ok"), false),
            (visible_history("root", "tampered output"), false),
        ] {
            let fixture = SourceContinuationFixture::new().await?;
            let assembled = fixture.assemble().await?;
            let server = server(&assembled)?;
            let first = continuation_reply(
                &server,
                &json!({"model":"bitrouter/coding","input":"root","stream":stream}),
            )
            .await?;
            let first_id = first["id"].as_str().context("root ID missing")?;
            assert!(first_id.starts_with("brc_"));
            fixture.reload(&assembled, false).await?;
            let tools = json!([{"type":"function","name":"inspect_repository",
                "parameters":{"type":"object","properties":{}}}]);
            let second = continuation_reply(
                &server,
                &json!({"model":"bitrouter/coding","input":input,
                "previous_response_id":first_id,"stream":stream,"instructions":"repository rules",
                "tools":tools,"tool_choice":"auto","parallel_tool_calls":false}),
            )
            .await?;
            let second_id = second["id"].as_str().context("followup ID missing")?;
            assert!(second_id.starts_with("brc_"));
            assert_ne!(first_id, second_id);
            let mut expected_item = json!({
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "ok"}],
            });
            if stream {
                // SSE assigns an identity to each message item for its lifecycle events.
                let first_item_id = first["output"][0]["id"]
                    .as_str()
                    .context("first streamed item ID missing")?;
                let second_item_id = second["output"][0]["id"]
                    .as_str()
                    .context("second streamed item ID missing")?;
                for id in [first_item_id, second_item_id] {
                    assert!(id.strip_prefix("msg_").is_some_and(|id| !id.is_empty()));
                }
                assert_ne!(first_item_id, second_item_id);
                expected_item["status"] = json!("completed");
                expected_item["id"] = json!(first_item_id);
                assert_eq!(first["output"], json!([expected_item]));
                expected_item["id"] = json!(second_item_id);
                assert_eq!(second["output"], json!([expected_item]));
            } else {
                assert_eq!(first["output"], json!([expected_item]));
                assert_eq!(second["output"], json!([expected_item]));
            }
            assert_eq!(second["usage"], first["usage"]);
            assert_eq!(second["status"], "completed");
            let (models, parents, bodies) = {
                let state = fixture
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Responses state poisoned"))?;
                (
                    state.served_models.clone(),
                    state.forwarded_parents.clone(),
                    state.forwarded_bodies.clone(),
                )
            };
            assert_eq!(
                models,
                [
                    "selected-model",
                    if detached {
                        "cheap-model"
                    } else {
                        "selected-model"
                    }
                ]
            );
            assert_eq!(
                parents,
                [None, (!detached).then(|| "provider-response-0".to_owned())]
            );
            let forwarded = bodies.get(1).context("followup was not forwarded")?;
            let expected_input = if let Some(text) = input.as_str() {
                json!([{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}])
            } else {
                input.clone()
            };
            assert_eq!(forwarded["input"], expected_input);
            assert_eq!(forwarded["instructions"], "repository rules");
            // The provider adapter renders an omitted optional description as
            // null; the complete tool schema and its order remain intact.
            let mut expected_tools = tools.clone();
            for tool in expected_tools
                .as_array_mut()
                .context("fixture tools must be an array")?
            {
                tool["description"] = Value::Null;
            }
            assert_eq!(forwarded["tools"], expected_tools);
            assert_eq!(forwarded["tool_choice"], "auto");
            assert_eq!(forwarded["parallel_tool_calls"], false);
            assert_eq!(
                fixture
                    .original
                    .received_requests()
                    .await
                    .context("original capture")?
                    .len(),
                if detached { 1 } else { 2 }
            );
            assert_eq!(
                fixture
                    .alternate
                    .received_requests()
                    .await
                    .context("alternate capture")?
                    .len(),
                usize::from(detached)
            );
            assembled
                .app
                .language_model()
                .context("pipeline")?
                .drain_required_pending_settlements()
                .await?;
            let rows = requests::Entity::find().all(&assembled.db).await?;
            assert_eq!(
                rows.len(),
                2,
                "continuation/detach must settle once per turn"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn current_source_stream_and_json_continuations_resume_after_restart_without_raw_ids()
-> anyhow::Result<()> {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    for stream in [false, true] {
        let fixture = SourceContinuationFixture::new().await?;
        let assembled = fixture.assemble().await?;
        let first_server = server(&assembled)?;
        let first = continuation_reply(
            &first_server,
            &json!({"model":"bitrouter/coding","input":"root","stream":stream}),
        )
        .await?;
        let first_id = first["id"].as_str().context("root ID missing")?.to_owned();
        assembled
            .app
            .language_model()
            .context("pipeline")?
            .drain_required_pending_settlements()
            .await?;
        let db = assembled.db.clone();
        drop(first_server);
        drop(assembled);
        db.close().await?;
        // Change the proposal explicitly while stopped. Hidden history still
        // belongs to the original selected serving authority after reopening.
        fixture.write_source("cheap", false)?;
        let restarted = fixture.assemble().await?;
        let second_server = server(&restarted)?;
        let second = continuation_reply(
            &second_server,
            &json!({"model":"bitrouter/coding","input":"continue",
            "stream":stream,"previous_response_id":first_id}),
        )
        .await?;
        assert!(
            second["id"]
                .as_str()
                .context("followup ID missing")?
                .starts_with("brc_")
        );
        restarted
            .app
            .language_model()
            .context("pipeline")?
            .drain_required_pending_settlements()
            .await?;
        let rows = restarted.db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT continuation_identity || owner_identity || COALESCE(ciphertext, '') || COALESCE(nonce, '') || target_fingerprint || key_id AS value FROM provider_continuations".to_owned())).await?;
        assert_eq!(rows.len(), 2);
        for row in rows {
            let value: String = row.try_get("", "value")?;
            assert!(!value.contains("provider-response-"));
        }
        {
            let state = fixture
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("Responses state poisoned"))?;
            assert_eq!(state.served_models, ["selected-model", "selected-model"]);
            assert_eq!(
                state.forwarded_parents,
                [None, Some("provider-response-0".to_owned())]
            );
        }
        assert!(
            fixture
                .alternate
                .received_requests()
                .await
                .context("alternate capture")?
                .is_empty()
        );
    }
    Ok(())
}

#[tokio::test]
async fn current_source_removed_continuation_target_and_invalid_request_id_fail_before_send()
-> anyhow::Result<()> {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    for stream in [false, true] {
        let fixture = SourceContinuationFixture::new().await?;
        let assembled = fixture.assemble().await?;
        let server = server(&assembled)?;
        server
            .post("/v1/responses")
            .add_header("x-bitrouter-request-id", "x".repeat(129))
            .json(&json!({"model":"bitrouter/coding","input":"root","stream":stream}))
            .await
            .assert_status_bad_request();
        assert!(
            fixture
                .original
                .received_requests()
                .await
                .context("original capture")?
                .is_empty()
        );
        assert!(
            fixture
                .alternate
                .received_requests()
                .await
                .context("alternate capture")?
                .is_empty()
        );
        let count = assembled
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .context("continuation count missing")?;
        assert_eq!(count.try_get::<i64>("", "count")?, 0);
        let first = continuation_reply(
            &server,
            &json!({"model":"bitrouter/coding","input":"root","stream":stream}),
        )
        .await?;
        fixture.reload(&assembled, true).await?;
        let response = server
            .post("/v1/responses")
            .json(&json!({"model":"bitrouter/coding","input":"continue",
            "previous_response_id":first["id"],"stream":stream}))
            .await;
        response.assert_status_bad_request();
        assert!(response.text().contains("unavailable in the active policy"));
        assert_eq!(
            fixture
                .original
                .received_requests()
                .await
                .context("original capture")?
                .len(),
            1
        );
        assert!(
            fixture
                .alternate
                .received_requests()
                .await
                .context("alternate capture")?
                .is_empty()
        );
    }
    Ok(())
}

#[tokio::test]
async fn current_source_hidden_suffix_requires_the_complete_committed_prefix_to_detach()
-> anyhow::Result<()> {
    for stream in [false, true] {
        for complete_prefix in [false, true] {
            let fixture = SourceContinuationFixture::new().await?;
            let assembled = fixture.assemble().await?;
            let server = server(&assembled)?;
            let first = continuation_reply(
                &server,
                &json!({"model":"bitrouter/coding","input":"root","stream":stream}),
            )
            .await?;
            fixture.reload(&assembled, false).await?;
            let second = continuation_reply(
                &server,
                &json!({"model":"bitrouter/coding","input":"hidden",
                "previous_response_id":first["id"],"stream":stream}),
            )
            .await?;
            let mut visible = if complete_prefix {
                vec![
                    json!({"type":"message","role":"user","content":[{"type":"input_text","text":"root"}]}),
                    json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}),
                ]
            } else {
                Vec::new()
            };
            visible.extend([
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"hidden"}]}),
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}),
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}),
            ]);
            continuation_reply(
                &server,
                &json!({"model":"bitrouter/coding","input":visible,
                "previous_response_id":second["id"],"stream":stream}),
            )
            .await?;
            let state = fixture
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("Responses state poisoned"))?;
            assert_eq!(
                state.served_models,
                [
                    "selected-model",
                    "selected-model",
                    if complete_prefix {
                        "cheap-model"
                    } else {
                        "selected-model"
                    }
                ]
            );
            assert_eq!(
                state.forwarded_parents,
                [
                    None,
                    Some("provider-response-0".to_owned()),
                    (!complete_prefix).then(|| "provider-response-1".to_owned())
                ]
            );
            assert_eq!(state.forwarded_bodies[2]["input"], json!(visible));
        }
    }
    Ok(())
}
