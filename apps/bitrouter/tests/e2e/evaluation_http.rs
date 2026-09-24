//! TypeSafe provider boundary exercised through real HTTP handlers.

use anyhow::Context;
use axum::Router;
use axum_test::TestServer;
use bitrouter::assemble::Assembled;
use bitrouter_sdk::config;
use bitrouter_sdk::extension::ExtensionApi;
use bitrouter_sdk::server::{AppState, RouterOptions, build_router_with_options};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture_config(base_url: &str, skip_auth: bool) -> anyhow::Result<config::Config> {
    Ok(config::parse(&format!(
        r#"
inherit_defaults: false
server:
  skip_auth: {skip_auth}
database:
  url: "sqlite::memory:"
providers:
  typesafe:
    api_base: {}
    api_key: fixture-secret
    operations:
      evaluate:
        endpoint: /v1/systemone
    models:
      - id: typesafe/jev-1.13
        provider_model_id: jev-1.13.0
        operations:
          evaluate:
            question_types: [noul, choice, score]
            max_choice_options: 255
            max_score_levels: 10
        pricing:
          input_micro_usd_per_token: 0.042
          output_micro_usd_per_token: 0
  legacy:
    api_base: {}
    api_key: fixture-secret
    models:
      - id: legacy/chat
"#,
        base_url, base_url
    ))?)
}

async fn server(upstream: &MockServer, skip_auth: bool) -> anyhow::Result<TestServer> {
    let config = fixture_config(&upstream.uri(), skip_auth)?;
    server_for_config(&config).await
}

async fn server_for_config(config: &config::Config) -> anyhow::Result<TestServer> {
    let mut extensions = ExtensionApi::new();
    bitrouter_typesafe_provider::register(&mut extensions)?;
    let assembled =
        bitrouter::assemble::build_app_with_registered_extensions(config, None, &extensions, None)
            .await?;
    Ok(TestServer::new(router_for_assembled(config, &assembled)?))
}

fn router_for_assembled(config: &config::Config, assembled: &Assembled) -> anyhow::Result<Router> {
    let state = AppState {
        language_model: assembled
            .app
            .language_model()
            .context("missing language-model pipeline")?
            .clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    };
    let router = build_router_with_options(
        state,
        RouterOptions {
            omit_v1_models: true,
            ..RouterOptions::default()
        },
    )
    .merge(bitrouter::evaluation_http::router(config, assembled));
    Ok(router)
}

#[tokio::test]
async fn custom_host_exposes_only_openrouter_shaped_evaluate_and_routable_model()
-> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", "Bearer fixture-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-1.13.0",
            "answers": {
                "approve": {"type": "noul", "noul": 0.94},
            },
            "usage": {"input_tokens": 318, "output_tokens": 34}
        })))
        .mount(&upstream)
        .await;
    let server = server(&upstream, true).await?;
    let listing: Value = server.get("/v1/models").await.json();
    let models = listing["data"]
        .as_array()
        .context("model listing missing data")?;
    assert!(models.iter().any(|model| {
        model["id"] == "typesafe/jev-1.13" && model["operations"] == json!(["evaluate"])
    }));
    assert!(
        !models
            .iter()
            .any(|model| model["id"] == "typesafe/jev-latest")
    );
    let request = json!({
        "model": "typesafe/jev-1.13",
        "state": {"order": 42},
        "questions": {"approve": {"type": "noul", "instructions": "approve?"}},
        "provider": {"order": ["untrusted"]},
        "user": "ignored",
        "session_id": "ignored",
        "trace": "ignored",
        "stream": true
    });
    let response = server
        .post("/v1/evaluate")
        .add_header("x-bitrouter-request-id", "eval-http-1")
        .json(&request)
        .await;
    response.assert_status_ok();
    let body: Value = response.json();
    assert_eq!(body["id"], "eval-http-1");
    assert_eq!(body["model"], "jev-1.13.0");
    assert_eq!(body["provider"], "typesafe");
    assert_eq!(body["answers"]["approve"]["noul"], 0.94);
    assert_eq!(body["usage"]["input_tokens"], 318);
    assert_eq!(body["usage"]["output_tokens"], 34);
    assert!(
        body["usage"]["cost"]
            .as_f64()
            .is_some_and(|cost| cost > 0.0)
    );
    let calls = upstream
        .received_requests()
        .await
        .context("mock upstream request capture unavailable")?;
    assert_eq!(calls.len(), 1);
    let sent: Value = serde_json::from_slice(&calls[0].body)?;
    assert_eq!(sent["model"], "jev-1.13.0");
    assert!(sent.get("provider").is_none());
    assert!(sent.get("user").is_none());
    assert!(sent.get("stream").is_none());
    let pinned = server
        .post("/v1/evaluate")
        .json(
            &json!({"model": "typesafe:typesafe/jev-1.13", "state": "test", "questions": {
                "approve": {"type": "noul", "instructions": "approve?"}
            }}),
        )
        .await;
    pinned.assert_status_ok();
    assert_eq!(pinned.json::<Value>()["provider"], "typesafe");
    assert_eq!(server.post("/v1/systemone").await.status_code(), 404);
    Ok(())
}

#[tokio::test]
async fn evaluation_route_and_model_listing_follow_a_validated_reload() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-1.13.0",
            "answers": {"approve": {"type": "noul", "noul": 0.8}},
            "usage": {"input_tokens": 8, "output_tokens": 0}
        })))
        .mount(&upstream)
        .await;
    let active = fixture_config(&upstream.uri(), true)?;
    let mut inactive = active.clone();
    inactive.providers.remove("typesafe");
    let mut extensions = ExtensionApi::new();
    bitrouter_typesafe_provider::register(&mut extensions)?;
    let assembled = bitrouter::assemble::build_app_with_registered_extensions(
        &inactive,
        None,
        &extensions,
        None,
    )
    .await?;
    let server = TestServer::new(router_for_assembled(&inactive, &assembled)?);
    let request = json!({
        "model": "typesafe/jev-1.13",
        "state": "synthetic",
        "questions": {"approve": {"type": "noul", "instructions": "approve?"}}
    });
    assert_eq!(
        server
            .post("/v1/evaluate")
            .json(&request)
            .await
            .status_code(),
        404
    );
    let before: Value = server.get("/v1/models").await.json();
    assert!(before["data"].as_array().is_some_and(|models| {
        models
            .iter()
            .all(|model| model["id"] != "typesafe/jev-1.13")
    }));

    extensions.validate_evaluation_bindings(&active)?;
    assembled
        .routing_table
        .replace_prepared_config(active)
        .await?;
    let after: Value = server.get("/v1/models").await.json();
    assert!(after["data"].as_array().is_some_and(|models| {
        models
            .iter()
            .any(|model| model["id"] == "typesafe/jev-1.13")
    }));
    server
        .post("/v1/evaluate")
        .json(&request)
        .await
        .assert_status_ok();
    Ok(())
}

#[tokio::test]
async fn evaluation_does_not_forward_a_bearer_across_redirects() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    let redirected = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/capture", redirected.uri())),
        )
        .mount(&upstream)
        .await;
    let server = server(&upstream, true).await?;
    let response = server
        .post("/v1/evaluate")
        .json(&json!({
            "model": "typesafe/jev-1.13",
            "state": "synthetic",
            "questions": {"approve": {"type": "noul", "instructions": "approve?"}}
        }))
        .await;
    assert_ne!(response.status_code(), 200);
    assert!(
        redirected
            .received_requests()
            .await
            .context("redirect capture unavailable")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn evaluation_auth_and_invalid_requests_fail_before_upstream() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    let protected = server(&upstream, false).await?;
    let request = json!({
        "model": "typesafe/jev-1.13",
        "state": "test",
        "questions": {"approve": {"type": "noul", "instructions": "approve?"}}
    });
    let unauthenticated = protected.post("/v1/evaluate").json(&request).await;
    assert_eq!(unauthenticated.status_code(), 401);
    let error: Value = unauthenticated.json();
    assert_eq!(error["error"]["code"], "unauthorized");
    let open = server(&upstream, true).await?;
    let invalid = open
        .post("/v1/evaluate")
        .json(&json!({"model": "typesafe/jev-1.13", "state": "test", "questions": {}}))
        .await;
    assert_eq!(invalid.status_code(), 400);
    assert_eq!(
        invalid.json::<Value>()["error"]["code"],
        "invalid_evaluation_request"
    );
    let unknown = open
        .post("/v1/evaluate")
        .json(
            &json!({"model": "typesafe/unknown", "state": "test", "questions": {
                "approve": {"type": "noul", "instructions": "approve?"}
            }}),
        )
        .await;
    assert_eq!(unknown.status_code(), 404);
    assert_eq!(
        unknown.json::<Value>()["error"]["code"],
        "evaluation_model_not_found"
    );
    let mismatch = open
        .post("/v1/evaluate")
        .json(
            &json!({"model": "legacy/chat", "state": "test", "questions": {
                "approve": {"type": "noul", "instructions": "approve?"}
            }}),
        )
        .await;
    assert_eq!(mismatch.status_code(), 409);
    assert_eq!(
        mismatch.json::<Value>()["error"]["code"],
        "model_operation_mismatch"
    );
    assert!(
        upstream
            .received_requests()
            .await
            .context("mock upstream request capture unavailable")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn authenticated_virtual_key_reaches_typesafe_without_generation_hooks() -> anyhow::Result<()>
{
    use bitrouter::auth::{NewApiKey, db as auth_db, generate};
    use bitrouter::metering::entities::evaluation_attempts;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-1.13.0",
            "answers": {"approve": {"type": "noul", "noul": 0.8}},
            "usage": {"input_tokens": 8, "output_tokens": 2}
        })))
        .mount(&upstream)
        .await;
    let config = fixture_config(&upstream.uri(), false)?;
    let mut extensions = ExtensionApi::new();
    bitrouter_typesafe_provider::register(&mut extensions)?;
    let assembled =
        bitrouter::assemble::build_app_with_registered_extensions(&config, None, &extensions, None)
            .await?;
    auth_db::upsert_user(&assembled.db, "evaluation-user").await?;
    let key = generate();
    auth_db::insert_api_key(
        &assembled.db,
        &NewApiKey {
            id: "evaluation-key".into(),
            key_hash: key.hash,
            user_id: "evaluation-user".into(),
            spend_limit_micro_usd: None,
            rpm_limit: None,
            policy_id: None,
        },
    )
    .await?;
    let server = TestServer::new(router_for_assembled(&config, &assembled)?);
    let response = server
        .post("/v1/evaluate")
        .add_header("x-bitrouter-request-id", "eval-auth-1")
        .add_header("authorization", format!("Bearer {}", key.secret))
        .json(&json!({
            "model": "typesafe:typesafe/jev-1.13",
            "state": "synthetic",
            "questions": {"approve": {"type": "noul", "instructions": "approve?"}}
        }))
        .await;
    response.assert_status_ok();
    assert_eq!(response.json::<Value>()["provider"], "typesafe");
    let record = evaluation_attempts::Entity::find()
        .filter(evaluation_attempts::Column::RequestId.eq("eval-auth-1"))
        .one(&assembled.db)
        .await?
        .context("authenticated evaluation attempt missing")?;
    assert_eq!(record.selector, "typesafe:typesafe/jev-1.13");
    assert_eq!(record.canonical_model.as_deref(), Some("typesafe/jev-1.13"));
    assert_eq!(record.provider_model_id, "jev-1.13.0");
    assert_eq!(record.caller_api_key_id.as_deref(), Some("evaluation-key"));
    assert_eq!(record.caller_user_id.as_deref(), Some("evaluation-user"));
    assert_eq!(
        upstream
            .received_requests()
            .await
            .context("mock upstream request capture unavailable")?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn typesafe_attempt_evidence_preserves_registry_pricing_origin() -> anyhow::Result<()> {
    use bitrouter::metering::entities::evaluation_attempts;
    use bitrouter_sdk::config::ModelPricingOrigin;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-1.13.0",
            "answers": {"approve": {"type": "noul", "noul": 0.8}},
            "usage": {"input_tokens": 318, "output_tokens": 1}
        })))
        .mount(&upstream)
        .await;
    let mut config = fixture_config(&upstream.uri(), true)?;
    let model = config
        .providers
        .get_mut("typesafe")
        .and_then(|provider| provider.models.first_mut())
        .context("TypeSafe fixture model missing")?;
    model.pricing_origin = ModelPricingOrigin::Registry;
    let mut extensions = ExtensionApi::new();
    bitrouter_typesafe_provider::register(&mut extensions)?;
    let assembled =
        bitrouter::assemble::build_app_with_registered_extensions(&config, None, &extensions, None)
            .await?;
    let server = TestServer::new(router_for_assembled(&config, &assembled)?);
    server
        .post("/v1/evaluate")
        .add_header("x-bitrouter-request-id", "eval-registry-price")
        .json(&json!({
            "model": "typesafe/jev-1.13",
            "state": "synthetic",
            "questions": {"approve": {"type": "noul", "instructions": "approve?"}}
        }))
        .await
        .assert_status_ok();
    let row = evaluation_attempts::Entity::find()
        .filter(evaluation_attempts::Column::RequestId.eq("eval-registry-price"))
        .one(&assembled.db)
        .await?
        .context("TypeSafe evaluation attempt missing")?;
    let evidence: Value = serde_json::from_str(
        row.charge_evidence_json
            .as_deref()
            .context("TypeSafe charge evidence missing")?,
    )?;
    assert_eq!(evidence["pricing_source"], "registry");
    assert!(row.charge_micro_usd.is_some_and(|charge| charge > 0));
    Ok(())
}

#[tokio::test]
async fn typesafe_retries_next_account_and_settles_once() -> anyhow::Result<()> {
    use bitrouter::metering::entities::evaluation_attempts;
    use bitrouter_sdk::config::{AccountStrategy, ProviderAccount};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", "Bearer first-secret"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", "Bearer second-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-1.13.0",
            "answers": {"approve": {"type": "noul", "noul": 0.8}},
            "usage": {"input_tokens": 318, "output_tokens": 2}
        })))
        .mount(&upstream)
        .await;
    let mut config = fixture_config(&upstream.uri(), true)?;
    let provider = config
        .providers
        .get_mut("typesafe")
        .context("TypeSafe fixture provider missing")?;
    provider.account_strategy = AccountStrategy::Failover;
    provider.accounts = [("first-secret", "first"), ("second-secret", "second")]
        .into_iter()
        .map(|(api_key, label)| ProviderAccount {
            api_key: api_key.into(),
            label: label.into(),
            ..ProviderAccount::default()
        })
        .collect();
    let mut extensions = ExtensionApi::new();
    bitrouter_typesafe_provider::register(&mut extensions)?;
    let assembled =
        bitrouter::assemble::build_app_with_registered_extensions(&config, None, &extensions, None)
            .await?;
    let server = TestServer::new(router_for_assembled(&config, &assembled)?);
    let response = server
        .post("/v1/evaluate")
        .add_header("x-bitrouter-request-id", "eval-typesafe-failover")
        .json(&json!({
            "model": "typesafe/jev-1.13",
            "state": "synthetic",
            "questions": {"approve": {"type": "noul", "instructions": "approve?"}}
        }))
        .await;
    response.assert_status_ok();
    let rows = evaluation_attempts::Entity::find()
        .filter(evaluation_attempts::Column::RequestId.eq("eval-typesafe-failover"))
        .all(&assembled.db)
        .await?;
    assert_eq!(rows.len(), 2);
    let first = rows.iter().find(|row| row.attempt_index == 1);
    let second = rows.iter().find(|row| row.attempt_index == 2);
    let first = first.context("first TypeSafe attempt missing")?;
    let second = second.context("second TypeSafe attempt missing")?;
    assert_eq!(first.account_label.as_deref(), Some("first"));
    assert_eq!(first.terminal, "failed");
    assert_eq!(first.error_code.as_deref(), Some("upstream_rate_limited"));
    assert_eq!(first.charge_micro_usd, None);
    assert_eq!(second.account_label.as_deref(), Some("second"));
    assert_eq!(second.terminal, "completed");
    assert!(second.charge_micro_usd.is_some_and(|charge| charge > 0));
    assert_eq!(
        upstream
            .received_requests()
            .await
            .context("mock upstream request capture unavailable")?
            .len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn discarded_question_limits_do_not_reject_valid_bounded_requests() -> anyhow::Result<()> {
    const QUESTION_COUNT: usize = 512;
    const ID_BYTES: usize = 1024;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let answers: serde_json::Map<String, Value> = body["questions"]
                .as_object()
                .into_iter()
                .flat_map(|questions| questions.keys())
                .map(|id| (id.clone(), json!({"type": "noul", "noul": 0.5})))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({
                "model": "jev-1.13.0",
                "answers": answers,
                "usage": {"input_tokens": 20, "output_tokens": 0}
            }))
        })
        .mount(&upstream)
        .await;
    let server = server(&upstream, true).await?;
    let long_id = "q".repeat(ID_BYTES);
    let mut questions = serde_json::Map::new();
    for index in 0..QUESTION_COUNT {
        let id = if index == 0 {
            long_id.clone()
        } else {
            format!("question-{index}")
        };
        questions.insert(id, json!({"type": "noul", "instructions": "check"}));
    }
    let response = server
        .post("/v1/evaluate")
        .json(&json!({"model": "typesafe/jev-1.13", "state": "bounded", "questions": questions}))
        .await;
    response.assert_status_ok();
    let result: Value = response.json();
    assert_eq!(
        result["answers"].as_object().map(|answers| answers.len()),
        Some(QUESTION_COUNT)
    );
    assert!(result["answers"].get(&long_id).is_some());
    let calls = upstream
        .received_requests()
        .await
        .context("mock upstream request capture unavailable")?;
    assert_eq!(calls.len(), 1);
    Ok(())
}

#[tokio::test]
async fn oversized_evaluation_body_is_rejected_before_upstream() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    let server = server(&upstream, true).await?;
    let response = server
        .post("/v1/evaluate")
        .json(&json!({
            "model": "typesafe/jev-1.13",
            "state": "x".repeat(16 * 1024 * 1024),
            "questions": {"approve": {"type": "noul", "instructions": "approve?"}}
        }))
        .await;
    assert_eq!(response.status_code(), 413);
    assert!(
        upstream
            .received_requests()
            .await
            .context("mock upstream request capture unavailable")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn upstream_failures_have_stable_safe_evaluation_errors() -> anyhow::Result<()> {
    let cases = [
        (401, 502, "upstream_authentication_failed"),
        (422, 422, "provider_rejected_evaluation"),
        (429, 429, "upstream_rate_limited"),
        (529, 503, "upstream_unavailable"),
    ];
    for (upstream_status, expected_status, expected_code) in cases {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(
                ResponseTemplate::new(upstream_status)
                    .insert_header("retry-after", "4")
                    .set_body_string("private upstream diagnostic: secret-value"),
            )
            .mount(&upstream)
            .await;
        let server = server(&upstream, true).await?;
        let response = server
            .post("/v1/evaluate")
            .json(&json!({
                "model": "typesafe/jev-1.13",
                "state": "synthetic",
                "questions": {"approve": {"type": "noul", "instructions": "approve?"}}
            }))
            .await;
        assert_eq!(response.status_code().as_u16(), expected_status);
        if upstream_status == 429 {
            assert_eq!(response.header("retry-after"), "4");
        }
        let body: Value = response.json();
        assert_eq!(body["error"]["code"], expected_code);
        assert!(!body.to_string().contains("secret-value"));
    }
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"invalid": true})))
        .mount(&upstream)
        .await;
    let server = server(&upstream, true).await?;
    let response = server
        .post("/v1/evaluate")
        .json(&json!({
            "model": "typesafe/jev-1.13",
            "state": "synthetic",
            "questions": {"approve": {"type": "noul", "instructions": "approve?"}}
        }))
        .await;
    assert_eq!(response.status_code(), 502);
    assert_eq!(
        response.json::<Value>()["error"]["code"],
        "upstream_invalid_response"
    );
    Ok(())
}

async fn run_typesafe_smoke(api_base: &str, key: &str) -> anyhow::Result<()> {
    use bitrouter::metering::entities::evaluation_attempts;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let mut config = fixture_config(api_base, true)?;
    let provider = config
        .providers
        .get_mut("typesafe")
        .context("TypeSafe fixture provider missing")?;
    provider.api_key = key.to_owned();
    let mut extensions = ExtensionApi::new();
    bitrouter_typesafe_provider::register(&mut extensions)?;
    let assembled =
        bitrouter::assemble::build_app_with_registered_extensions(&config, None, &extensions, None)
            .await?;
    let server = TestServer::new(router_for_assembled(&config, &assembled)?);
    let cases = [
        (
            "noul",
            json!({"type": "noul", "instructions": "Is this ticket about billing?"}),
        ),
        (
            "choice",
            json!({
                "type": "choice",
                "instructions": "Which team handles this ticket?",
                "criteria": {"billing": null, "technical": null}
            }),
        ),
        (
            "score",
            json!({
                "type": "score",
                "instructions": "How urgent is this ticket?",
                "criteria": ["routine", "urgent"]
            }),
        ),
    ];
    for (kind, question) in cases {
        let request_id = format!("eval-typesafe-live-{kind}");
        let mut questions = serde_json::Map::new();
        questions.insert(kind.to_owned(), question);
        let response = server
            .post("/v1/evaluate")
            .add_header("x-bitrouter-request-id", request_id.as_str())
            .json(&json!({
                "model": "typesafe/jev-1.13",
                "state": {"ticket": "A synthetic customer reports a duplicate charge."},
                "questions": questions
            }))
            .await;
        anyhow::ensure!(
            response.status_code() == 200,
            "live TypeSafe {kind} evaluation failed with HTTP {}",
            response.status_code()
        );
        let body: Value = response.json();
        let model = body["model"]
            .as_str()
            .context("provider model version missing")?;
        anyhow::ensure!(model.starts_with("jev-"), "provider did not report Jev");
        anyhow::ensure!(body["provider"] == "typesafe", "provider identity mismatch");
        anyhow::ensure!(
            body["answers"][kind]["type"] == kind,
            "{kind} answer missing"
        );
        anyhow::ensure!(
            body["usage"]["input_tokens"]
                .as_u64()
                .is_some_and(|tokens| tokens > 0),
            "input usage missing for {kind}"
        );
        anyhow::ensure!(
            body["usage"]["output_tokens"].as_u64().is_some(),
            "output usage missing for {kind}"
        );
        let cost = body["usage"]["cost"]
            .as_f64()
            .context("settled cost missing")?;
        let row = evaluation_attempts::Entity::find()
            .filter(evaluation_attempts::Column::RequestId.eq(request_id))
            .one(&assembled.db)
            .await?
            .context("live TypeSafe attempt evidence missing")?;
        anyhow::ensure!(row.terminal == "completed", "attempt did not complete");
        anyhow::ensure!(
            row.reported_model.as_deref() == Some(model),
            "reported model drift"
        );
        anyhow::ensure!(row.provider_model_id == "jev-1.13.0", "wire model drift");
        anyhow::ensure!(row.charge_status == "computed", "charge not computed");
        let charge = row.charge_micro_usd.context("settled charge missing")?;
        anyhow::ensure!(
            (cost - charge as f64 / 1_000_000.0).abs() < 1e-9,
            "response cost differs from settlement"
        );
        let evidence: Value = serde_json::from_str(
            row.charge_evidence_json
                .as_deref()
                .context("charge evidence missing")?,
        )?;
        anyhow::ensure!(
            evidence["pricing_source"] == "configured",
            "unexpected pricing provenance"
        );
        anyhow::ensure!(
            evidence["pricing_version"].as_str().is_some(),
            "pricing version missing"
        );
        anyhow::ensure!(
            !format!("{row:?}{body}{evidence}").contains(key),
            "TypeSafe key appeared in response or settlement evidence"
        );
        println!("TypeSafe {kind} smoke passed for model {model}");
    }
    Ok(())
}

#[tokio::test]
async fn typesafe_smoke_contract_covers_each_kind_and_settlement() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", "Bearer fixture-live-key"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let kind = body["questions"]
                .as_object()
                .and_then(|questions| questions.keys().next())
                .map(String::as_str);
            let answer = match kind {
                Some("noul") => json!({"type": "noul", "noul": 0.8}),
                Some("choice") => json!({
                    "type": "choice",
                    "choice": "billing",
                    "probabilities": {"billing": 0.7, "technical": 0.3}
                }),
                Some("score") => json!({
                    "type": "score",
                    "score": 1.0,
                    "probabilities": {"0": 0.2, "1": 0.8},
                    "legend": {"0": "routine", "1": "urgent"}
                }),
                _ => return ResponseTemplate::new(400),
            };
            let mut answers = serde_json::Map::new();
            if let Some(kind) = kind {
                answers.insert(kind.to_owned(), answer);
            }
            ResponseTemplate::new(200).set_body_json(json!({
                "model": "jev-1.13.0",
                "answers": answers,
                "usage": {"input_tokens": 318, "output_tokens": 34}
            }))
        })
        .mount(&upstream)
        .await;
    run_typesafe_smoke(&upstream.uri(), "fixture-live-key").await?;
    anyhow::ensure!(
        upstream
            .received_requests()
            .await
            .context("mock upstream request capture unavailable")?
            .len()
            == 3,
        "smoke contract did not make one request per question kind"
    );
    Ok(())
}

/// Explicit, credentialed conformance probe. Ordinary PR CI never executes it.
/// Run with `TYPESAFE_API_KEY_FILE` or `TYPESAFE_API_KEY` set and `cargo test -p bitrouter --test e2e
/// typesafe_live_smoke -- --ignored` after the deterministic gate is green.
#[tokio::test]
#[ignore = "requires an explicit TypeSafe API key and spends up to three real provider calls"]
async fn typesafe_live_smoke() -> anyhow::Result<()> {
    let key = match std::env::var("TYPESAFE_API_KEY_FILE") {
        Ok(path) => std::fs::read_to_string(path).context("read TypeSafe smoke key file")?,
        Err(_) => std::env::var("TYPESAFE_API_KEY")
            .context("set TYPESAFE_API_KEY_FILE or TYPESAFE_API_KEY for live smoke")?,
    };
    let key = key.trim();
    anyhow::ensure!(!key.is_empty(), "TYPESAFE_API_KEY is empty");
    run_typesafe_smoke("https://api.typesafe.ai", key).await
}
