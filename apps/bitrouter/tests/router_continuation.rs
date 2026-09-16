//! Native Responses continuation coverage for named policy routers.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum_test::TestServer;
use bitrouter::metering::entities::requests;
use bitrouter_sdk::config;
use bitrouter_sdk::server::{AppState, build_router};
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

        ResponseTemplate::new(200)
            .insert_header("content-type", "application/json")
            .set_body_json(json!({
                "id": response_id,
                "object": "response",
                "status": "completed",
                "model": model,
                "output": [{
                    "id": "provider-item",
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "ok"}]
                }],
                "usage": {"input_tokens": 11, "output_tokens": 7, "total_tokens": 18}
            }))
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
    let policy_path = home.path().join("policy-lock.yaml");
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
  path: ./policy-lock.yaml
  mode: frozen
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
    let policy_text = r#"lockfileVersion: 1
policies:
  coding:
    tiers:
      strong: responses:selected-model
    default_tier: strong
    tool_use_tier: strong
    tool_safe_tiers: [strong]
"#;
    tokio::fs::write(&config_path, &config_text).await?;
    tokio::fs::write(&policy_path, policy_text).await?;

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
