//! The same checked JSON/SSE execution in each independent routing composition.

#[path = "routing_composition/processes.rs"]
mod processes;

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use axum_test::TestServer;
use bitrouter::config::{self, Config};
use bitrouter::gateway::http::{AppState, build_router};
use bitrouter_ai::extension::request_check::Decision;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn configuration(
    directory: &std::path::Path,
    endpoint: &str,
    policy: bool,
    disabled: bool,
) -> Result<Config> {
    let selection = if policy {
        json!({"kind": "policy", "policy": "fixture", "base_model": "fixture:base"})
    } else {
        json!({"kind": "model", "model": "fixture:selected"})
    };
    let mut value = json!({
        "inherit_defaults": false,
        "registry": {"enabled": false},
        "server": {"skip_auth": true},
        "database": {"url": format!("sqlite://{}?mode=rwc", directory.join("requests.db").display())},
        "providers": {"fixture": {
            "api_base": endpoint, "api_key": "fixture-token",
            "api_protocol": [{"*": "chat_completions"}],
            "models": [{"id": "base"}, {"id": "selected"}]
        }},
        "checkers": {"guard": {"native": {"revision": "fixture-v1"}}},
        "routers": {"checked": {"selection": selection, "checks": {"request": [{"checker": "guard"}]}}}
    });
    if disabled {
        value["inference_routing"] = json!({"enabled": false});
    }
    if policy {
        value["policy"] = json!({"path": directory.join("policy-lock.yaml")});
    }
    Ok(serde_json::from_value(value)?)
}

async fn exercise(
    policy: bool,
    disabled: bool,
    scheduling: bool,
    authenticated: bool,
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = MockServer::start().await;
    let mut config = configuration(directory.path(), &provider.uri(), policy, disabled)?;
    config.server.skip_auth = !authenticated;
    if scheduling {
        config.inference_routing = serde_json::from_value(
            json!({"enabled": true, "destinations": [{
                "id": "fixture", "provider": "fixture", "model": "selected", "endpoint": provider.uri()
            }]}),
        )?;
    }
    if policy {
        let mut lock: bitrouter::policy_lock::PolicyLock = serde_saphyr::from_str(
            "lockfileVersion: 4\npolicies:\n  fixture:\n    tiers:\n      strong: {model: 'fixture:selected', context: evidence}\n    default_tier: strong\n    tool_use_tier: strong\n    tool_safe_tiers: [strong]\n",
        )?;
        lock.artifact = bitrouter::policy_lock::PolicyLock::default().artifact;
        std::fs::write(
            directory.path().join("policy-lock.yaml"),
            bitrouter::policy_lock::deterministic_yaml(&lock)?,
        )?;
    } else if !cfg!(feature = "semantic-routing") {
        // Gateway-only assembly must not open an adjacent semantic artifact.
        std::fs::write(directory.path().join("policy-lock.yaml"), "invalid: [")?;
    }
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"model": "selected", "stream": false})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "json-result", "object": "chat.completion", "model": "selected",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ready"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 1, "total_tokens": 5}
        })))
        .expect(3).mount(&provider).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"model": "selected", "stream": true})))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(concat!(
            "data: {\"id\":\"stream-result\",\"object\":\"chat.completion.chunk\",\"model\":\"selected\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"stream-result\",\"object\":\"chat.completion.chunk\",\"model\":\"selected\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ready\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"stream-result\",\"object\":\"chat.completion.chunk\",\"model\":\"selected\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":1,\"total_tokens\":5}}\n\n",
            "data: [DONE]\n\n"
        )))
        .expect(3).mount(&provider).await;
    let assembled = bitrouter::assemble::build_app_with_extensions(
        &config,
        Some(&directory.path().join("bitrouter.yaml")),
        |api| {
            api.request_check(
                "guard",
                "fixture-v1",
                Arc::new(|input| {
                    if input
                        .content
                        .iter()
                        .any(|fragment| fragment.text.as_deref() == Some("deny"))
                    {
                        Decision::Deny {
                            reason_code: "fixture.denied".into(),
                        }
                    } else {
                        Decision::Allow
                    }
                }),
            )?;
            Ok(())
        },
    )
    .await?;
    let credential = if authenticated {
        let user = "composition-user";
        bitrouter::auth::db::upsert_user(&assembled.db, user).await?;
        let key = bitrouter::auth::keys::generate();
        bitrouter::auth::db::insert_api_key(
            &assembled.db,
            &bitrouter::auth::db::NewApiKey {
                id: "composition-key".into(),
                key_hash: key.hash,
                user_id: user.into(),
                spend_limit_micro_usd: None,
                rpm_limit: None,
                policy_id: None,
            },
        )
        .await?;
        Some(key.secret)
    } else {
        None
    };
    ensure!(assembled.app.decision_model().is_none());
    let pipeline = assembled
        .app
        .language_model()
        .context("missing pipeline")?
        .clone();
    let server = TestServer::new(build_router(AppState {
        language_model: pipeline.clone(),
        mcp: None,
        skip_auth: config.server.skip_auth,
        metrics_renderer: None,
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    }));
    let post = |path: &str, payload: Value| {
        let request = server.post(path).json(&payload);
        match &credential {
            Some(key) => request.authorization_bearer(key),
            None => request,
        }
    };
    if authenticated {
        let denied = server.post("/v1/chat/completions").json(&json!({"model":"bitrouter/checked","messages":[{"role":"user","content":"hello"}]})).await;
        ensure!(denied.status_code().as_u16() == 401);
    }
    let denied = post(
        "/v1/chat/completions",
        json!({"model":"bitrouter/checked","messages":[{"role":"user","content":"deny"}]}),
    )
    .await;
    ensure!(denied.status_code().is_client_error());
    ensure!(
        provider
            .received_requests()
            .await
            .context("provider journal")?
            .is_empty()
    );
    for endpoint in ["chat/completions", "messages", "responses"] {
        for streaming in [false, true] {
            let payload = if endpoint == "responses" {
                json!({"model":"bitrouter/checked", "stream":streaming, "input":"hello", "max_output_tokens":32})
            } else {
                json!({"model":"bitrouter/checked","stream":streaming,"messages":[{"role":"user","content":"hello"}],"max_tokens":32})
            };
            let response = post(&format!("/v1/{endpoint}"), payload).await;
            if !response.status_code().is_success() {
                eprintln!("composition {endpoint} response: {}", response.text());
            }
            ensure!(response.status_code().is_success(), "{}", response.text());
            let body = response.text();
            ensure!(body.contains("ready"), "{body}");
            if streaming && endpoint == "chat/completions" {
                ensure!(body.matches("[DONE]").count() == 1);
            }
            if streaming && endpoint == "messages" {
                ensure!(body.contains("message_stop"));
            }
            if streaming && endpoint == "responses" {
                ensure!(body.contains("response.completed"));
            }
            pipeline.drain_pending_settlements().await;
        }
    }
    pipeline.drain_required_pending_settlements().await?;
    if scheduling {
        let rejected = server.post("/v1/chat/completions").json(&json!({"model":"bitrouter/checked","messages":[{"role":"user","content":"after shutdown"}]})).await;
        ensure!(rejected.status_code().is_server_error());
    }
    ensure!(
        provider
            .received_requests()
            .await
            .context("provider journal")?
            .len()
            == 6
    );
    Ok(())
}

#[tokio::test]
async fn static_checks_json_and_sse_keep_legacy_execution() -> Result<()> {
    exercise(false, false, false, false).await?;
    exercise(false, true, false, false).await
}

#[cfg(feature = "semantic-routing")]
#[tokio::test]
async fn semantic_checks_json_and_sse_keep_model_decisions() -> Result<()> {
    exercise(true, false, false, false).await?;
    exercise(true, true, false, false).await
}

#[test]
fn unavailable_routing_features_fail_before_resources_or_credentials() -> Result<()> {
    for yaml in [
        "decision_model: {model: fixture, api_key_env: MUST_NOT_BE_READ}",
        "policy: {path: must-not-be-opened.yaml}",
        "routers: {test: {selection: {kind: policy, policy: fixture, base_model: 'fixture:model'}}}",
    ] {
        if !cfg!(feature = "semantic-routing") {
            let error = config::parse_with(yaml, |_| None)
                .err()
                .context("semantic config was accepted")?;
            ensure!(error.to_string().contains("semantic-routing"), "{error}");
        }
    }
    let error = config::parse_with("inference_routing: {enabled: true}", |_| None)
        .err()
        .context("invalid scheduler activated")?;
    let expected = if cfg!(feature = "inference-routing") {
        "at least one destination"
    } else {
        "inference-routing feature"
    };
    ensure!(error.to_string().contains(expected), "{error}");
    Ok(())
}

#[test]
fn default_semantic_policy_follows_build_capability() {
    let mut config = Config::default();
    bitrouter::policy_lock::apply_defaults(&mut config, None);
    assert_eq!(
        config.routers.contains_key("auto"),
        cfg!(feature = "semantic-routing")
    );
    assert!(!config.inference_routing.enabled);
}

#[test]
fn inference_configuration_cases_match_parser_and_startup_validation() -> Result<()> {
    let example: Value = serde_json::from_str(include_str!(
        "../../../docs/specs/configuration/examples/inference-routing.json"
    ))?;
    let staged: Config = serde_json::from_value(json!({"inference_routing": example}))?;
    staged.validate_router_config()?;
    let cases: Vec<Value> = serde_json::from_str(include_str!(
        "../../../docs/specs/configuration/cases/inference-routing.json"
    ))?;
    for case in cases {
        let config = serde_json::from_value::<Config>(json!({
            "inherit_defaults": false, "inference_routing": case["value"]
        }));
        let valid = match config {
            Ok(config) => config.validate_router_config().is_ok(),
            Err(_) => false,
        };
        ensure!(
            valid
                == (case["runtime_valid"]
                    .as_bool()
                    .context("missing runtime expectation")?
                    && (!case["value"]["enabled"].as_bool().unwrap_or(false)
                        || cfg!(feature = "inference-routing"))),
            "{}",
            case["name"]
        );
    }
    let old = config::parse_with("inherit_defaults: false", |_| None)?;
    let disabled = config::parse_with("inherit_defaults: false\ninference_routing: {}", |_| None)?;
    ensure!(old.inference_routing == disabled.inference_routing);
    Ok(())
}

#[cfg(feature = "inference-routing")]
#[tokio::test]
async fn enabled_scheduling_serves_json_and_sse_without_harness() -> Result<()> {
    exercise(false, false, true, false).await?;
    if cfg!(feature = "semantic-routing") {
        exercise(true, false, true, false).await?;
    }
    Ok(())
}

#[cfg(feature = "inference-routing")]
#[tokio::test]
async fn scheduler_hot_reload_rejects_mixed_cold_changes_atomically() -> Result<()> {
    use bitrouter::{
        daemon::DaemonReloader,
        policy::PolicyStore,
        reload::{AppReloader, ReloadSource},
    };
    let directory = tempfile::tempdir()?;
    let provider = MockServer::start().await;
    let path = directory.path().join("bitrouter.yaml");
    let mut value = serde_json::json!({
        "inherit_defaults":false,"registry":{"enabled":false},
        "providers":{"fixture":{"api_base":provider.uri(),"api_key":"test","api_protocol":[{"*":"chat_completions"}],"models":[{"id":"selected"}]}},
        "inference_routing":{"enabled":false,"destinations":[{"id":"fixture","provider":"fixture","model":"selected","endpoint":provider.uri()}]}
    });
    std::fs::write(&path, serde_json::to_vec(&value)?)?;
    let initial = config::load(&path).await?;
    let table = Arc::new(config::routing_table::ConfigRoutingTable::from_config(
        initial,
    ));
    let executor = Arc::new(bitrouter::gateway::pipeline::executor::HttpExecutor::new(
        Default::default(),
    )?);
    let reloader = AppReloader::new(
        Arc::new(PolicyStore::new()),
        table.clone(),
        executor,
        ReloadSource::File(path.clone()),
    );
    value["inference_routing"]["enabled"] = json!(true);
    value["inference_routing"]["policy"] = json!("prefix_affinity");
    value["inference_routing"]["tenant_weights"] = json!({"caller":3});
    std::fs::write(&path, serde_json::to_vec(&value)?)?;
    reloader
        .reload()
        .await
        .with_context(|| format!("reload state: {:?}", reloader.reload_state()))?;
    let (generation, enabled) = table.versioned_snapshot();
    ensure!(enabled.inference_routing.enabled);
    value["inference_routing"]["enabled"] = json!(false);
    value["inference_routing"]["limits"] = json!({"max_active":256});
    std::fs::write(&path, serde_json::to_vec(&value)?)?;
    ensure!(reloader.reload().await.is_err());
    ensure!(table.versioned_snapshot().0 == generation);
    ensure!(table.snapshot_config().inference_routing == enabled.inference_routing);
    value["inference_routing"]
        .as_object_mut()
        .context("inference configuration")?
        .remove("limits");
    std::fs::write(&path, serde_json::to_vec(&value)?)?;
    reloader
        .reload()
        .await
        .with_context(|| format!("reload state: {:?}", reloader.reload_state()))?;
    ensure!(!table.snapshot_config().inference_routing.enabled);
    Ok(())
}

#[cfg(feature = "inference-routing")]
#[test]
fn enabled_destinations_require_exact_native_protocol_bindings() -> Result<()> {
    let mut config: Config = serde_json::from_value(json!({
        "inherit_defaults":false,"registry":{"enabled":false},
        "providers":{"fixture":{"api_base":"https://fixture.example/v1","api_key":"test","api_protocol":[{"*":["chat_completions","messages"]}],"protocol_endpoints":{"messages":"https://fixture.example/anthropic"},"models":[{"id":"selected"}]}},
        "inference_routing":{"enabled":true,"destinations":[{"id":"fixture","provider":"fixture","model":"selected","endpoint":"https://fixture.example/anthropic"}]}
    }))?;
    bitrouter::assemble::validate_host_configuration(&config)?;
    config.inference_routing.destinations[0].endpoint = "https://unknown.example/v1".into();
    ensure!(bitrouter::assemble::validate_host_configuration(&config).is_err());
    Ok(())
}

#[cfg(feature = "inference-routing")]
#[tokio::test]
async fn authenticated_scheduling_serves_three_protocols_json_and_sse() -> Result<()> {
    exercise(false, false, true, true).await?;
    if cfg!(feature = "semantic-routing") {
        exercise(true, false, true, true).await?;
    }
    Ok(())
}
