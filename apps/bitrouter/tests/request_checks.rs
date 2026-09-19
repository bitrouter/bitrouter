//! Request checks exercised through the shipped host and HTTP gateway.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use axum_test::TestServer;
use bitrouter_sdk::config;
use bitrouter_sdk::extension::request_check::{Callback, Decision};
use bitrouter_sdk::server::{AppState, build_router};
use serde_json::{Value, json};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Clone, Default)]
struct Capture(Arc<std::sync::Mutex<Vec<Captured>>>);
#[derive(Clone)]
struct Captured {
    id: String,
    body: String,
}
impl Capture {
    async fn received_requests(&self) -> Option<Vec<Captured>> {
        self.0.lock().ok().map(|rows| rows.clone())
    }
    fn callback(&self, id: &'static str) -> Arc<Callback> {
        let rows = self.0.clone();
        Arc::new(move |input| {
            let body = input
                .content
                .iter()
                .filter_map(|part| part.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n");
            if let Ok(mut rows) = rows.lock() {
                rows.push(Captured {
                    id: id.to_owned(),
                    body,
                });
            }
            if id == "second" {
                Decision::Deny {
                    reason_code: "fixture.denied".to_owned(),
                }
            } else {
                Decision::Allow
            }
        })
    }
}
async fn assemble(config: &config::Config, capture: &Capture) -> Result<bitrouter::Assembled> {
    bitrouter::assemble::build_app_with_extensions(config, None, |api| {
        api.request_check("first", "rules-v1", capture.callback("first"))?;
        Ok(api.request_check("second", "rules-v1", capture.callback("second"))?)
    })
    .await
}

fn gateway(assembled: &bitrouter::Assembled) -> Result<TestServer> {
    Ok(TestServer::new(build_router(AppState {
        language_model: assembled
            .app
            .language_model()
            .context("no pipeline")?
            .clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    })))
}

fn configuration(upstream: &str, timeout: u64) -> Result<config::Config> {
    config::parse(&format!(
        r#"inherit_defaults: false
server:
  skip_auth: true
database:
  url: 'sqlite::memory:'
providers:
  fixture:
    api_base: {upstream}
    api_key: provider-secret-must-not-be-projected
    models:
      - id: model
checkers:
  first:
    native:
      revision: rules-v1
  second:
    native:
      revision: rules-v1
routers:
  coding:
    selection:
      kind: model
      model: fixture:model
    defaults:
      system_prompt: router-default-must-be-checked
    checks:
      request:
        - checker: first
          timeout_ms: {timeout}
  restricted:
    selection:
      kind: model
      model: fixture:model
    checks:
      request:
        - checker: second
          timeout_ms: {timeout}
"#
    ))
    .map_err(Into::into)
}

async fn mount_upstream(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "fixture-response",
            "object": "chat.completion",
            "model": "model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
        })))
        .mount(upstream)
        .await;
}

fn request(router: &str) -> Value {
    json!({"model": format!("bitrouter/{router}"), "messages": [{"role": "user", "content": "same input"}]})
}

#[tokio::test]
async fn routers_apply_distinct_checks_to_effective_text_before_model_dispatch() -> Result<()> {
    let upstream = MockServer::start().await;
    let checker = Capture::default();
    mount_upstream(&upstream).await;
    let config = configuration(&upstream.uri(), 5_000)?;
    let assembled = assemble(&config, &checker).await?;
    let server = gateway(&assembled)?;

    let allowed = server
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "check-allowed")
        .json(&request("coding"))
        .await;
    ensure!(
        allowed.status_code().is_success(),
        "allow failed: {}",
        allowed.text()
    );
    ensure!(allowed.header("x-bitrouter-request-id") == "check-allowed");
    let denied = server
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "check-denied")
        .json(&request("restricted"))
        .await;
    ensure!(
        !denied.status_code().is_success(),
        "deny unexpectedly succeeded"
    );
    ensure!(denied.header("x-bitrouter-request-id") == "check-denied");

    let calls = upstream
        .received_requests()
        .await
        .context("upstream capture unavailable")?;
    ensure!(calls.len() == 1, "denied request reached upstream");
    let checked = checker
        .received_requests()
        .await
        .context("checker capture unavailable")?;
    ensure!(checked.len() == 2);
    let allowed_check = checked
        .iter()
        .find(|call| call.id == "first")
        .context("no allow check")?;
    let body = allowed_check.body.clone();
    ensure!(body.contains("router-default-must-be-checked"));
    ensure!(body.contains("same input"));
    ensure!(!body.contains("provider-secret-must-not-be-projected"));
    Ok(())
}

#[tokio::test]
async fn checker_denial_stops_later_checker_and_model_dispatch() -> Result<()> {
    let upstream = MockServer::start().await;
    let checker = Capture::default();
    let mut config = configuration(&upstream.uri(), 5_000)?;
    let restricted = config
        .routers
        .get_mut("restricted")
        .context("missing restricted router")?;
    restricted
        .checks
        .request
        .push(config::router::RouterRequestCheck {
            checker: "first".to_owned(),
            timeout_ms: 5_000,
            max_input_bytes: config::router::DEFAULT_CHECKER_MAX_INPUT_BYTES,
        });
    let assembled = assemble(&config, &checker).await?;
    let server = gateway(&assembled)?;
    let response = server
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "checker-rejected")
        .json(&request("restricted"))
        .await;
    ensure!(!response.status_code().is_success());
    let checks = checker
        .received_requests()
        .await
        .context("checker capture unavailable")?;
    ensure!(checks.len() == 1, "later checker ran after denial");
    ensure!(checks.first().is_some_and(|request| request.id == "second"));
    ensure!(
        upstream
            .received_requests()
            .await
            .context("upstream capture unavailable")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn registration_is_inert_and_allow_does_not_mask_upstream_failure() -> Result<()> {
    let upstream = MockServer::start().await;
    let checker = Capture::default();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&upstream)
        .await;
    let assembled = assemble(&configuration(&upstream.uri(), 5_000)?, &checker).await?;
    ensure!(
        upstream
            .received_requests()
            .await
            .context("upstream capture unavailable")?
            .is_empty()
    );
    ensure!(
        checker
            .received_requests()
            .await
            .context("checker capture unavailable")?
            .is_empty()
    );
    let server = gateway(&assembled)?;
    let response = server
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "upstream-failed-after-allow")
        .json(&request("coding"))
        .await;
    ensure!(!response.status_code().is_success());
    ensure!(
        checker
            .received_requests()
            .await
            .context("checker capture unavailable")?
            .len()
            == 1
    );
    Ok(())
}

#[tokio::test]
async fn oversize_text_is_rejected_without_checker_or_model_dispatch() -> Result<()> {
    let upstream = MockServer::start().await;
    let checker = Capture::default();
    let mut config = configuration(&upstream.uri(), 5_000)?;
    let router = config
        .routers
        .get_mut("coding")
        .context("missing coding router")?;
    router.checks.request[0].max_input_bytes = 1024;
    let assembled = assemble(&config, &checker).await?;
    let server = gateway(&assembled)?;
    let mut body = request("coding");
    body["messages"][0]["content"] = Value::String("x".repeat(2048));
    let response = server
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "input-too-large")
        .json(&body)
        .await;
    ensure!(!response.status_code().is_success());
    ensure!(
        checker
            .received_requests()
            .await
            .context("checker capture unavailable")?
            .is_empty()
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("upstream capture unavailable")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn unauthenticated_requests_do_not_invoke_a_checker_or_claim_router_admission() -> Result<()>
{
    let upstream = MockServer::start().await;
    let checker = Capture::default();
    let mut config = configuration(&upstream.uri(), 5_000)?;
    config.server.skip_auth = false;
    let assembled = assemble(&config, &checker).await?;
    let server = gateway(&assembled)?;
    let response = server
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "unauthenticated")
        .json(&request("coding"))
        .await;
    ensure!(!response.status_code().is_success());
    ensure!(
        checker
            .received_requests()
            .await
            .context("checker capture unavailable")?
            .is_empty()
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("upstream capture unavailable")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn streaming_success_runs_native_check_before_delivery() -> Result<()> {
    let upstream = MockServer::start().await;
    let checker = Capture::default();
    let first = json!({"id":"stream-1","object":"chat.completion.chunk","model":"model","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]});
    let last = json!({"id":"stream-1","object":"chat.completion.chunk","model":"model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}});
    Mock::given(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        ))
        .mount(&upstream)
        .await;
    let assembled = assemble(&configuration(&upstream.uri(), 5_000)?, &checker).await?;
    let server = gateway(&assembled)?;
    let mut body = request("coding");
    body["stream"] = Value::Bool(true);
    let response = tokio::time::timeout(Duration::from_secs(10), async {
        server
            .post("/v1/chat/completions")
            .add_header("x-bitrouter-request-id", "stream-completed")
            .json(&body)
            .await
    })
    .await?;
    ensure!(response.status_code().is_success(), "{}", response.text());
    ensure!(response.text().contains("[DONE]"));
    assembled
        .app
        .language_model()
        .context("no pipeline")?
        .drain_required_pending_settlements()
        .await?;
    ensure!(
        checker
            .received_requests()
            .await
            .context("checker capture unavailable")?
            .len()
            == 1
    );
    Ok(())
}

#[tokio::test]
async fn repeated_transport_requests_each_run_native_checks() -> Result<()> {
    let upstream = MockServer::start().await;
    let checker = Capture::default();
    mount_upstream(&upstream).await;
    let assembled = assemble(&configuration(&upstream.uri(), 5_000)?, &checker).await?;
    let server = gateway(&assembled)?;
    for _ in 0..2 {
        let response = server
            .post("/v1/chat/completions")
            .add_header("x-bitrouter-request-id", "transport-retry")
            .json(&request("coding"))
            .await;
        ensure!(response.status_code().is_success(), "{}", response.text());
    }
    ensure!(
        checker
            .received_requests()
            .await
            .context("checker capture unavailable")?
            .len()
            == 2
    );
    Ok(())
}

fn native_config(config: &mut config::Config, id: &str, revision: &str) {
    config.checkers.insert(
        id.to_owned(),
        config::checker::CheckerConfig::Native {
            native: config::checker::NativeCheckerConfig {
                revision: revision.to_owned(),
            },
        },
    );
}

#[tokio::test]
async fn regex_extensions_preserve_decisions_and_router_bindings() -> Result<()> {
    use bitrouter_guardrails::{checker, config::InputGuardrailConfig};

    let rules: InputGuardrailConfig = serde_json::from_value(json!({
        "scope": "input", "rules": [{"name": "secret", "pattern": "secret-\\n[0-9]+", "action": "block"}]
    }))?;
    let callback = checker::callback(rules.compile()?);
    let upstream = MockServer::start().await;
    mount_upstream(&upstream).await;
    let mut config = configuration(&upstream.uri(), 5_000)?;
    native_config(&mut config, "first", "rules-v1");
    let assembled = bitrouter::assemble::build_app_with_extensions(&config, None, |api| {
        api.request_check("first", "rules-v1", callback.clone())?;
        Ok(api.request_check("second", "rules-v1", callback)?)
    })
    .await?;
    let server = gateway(&assembled)?;
    for router in ["coding", "restricted"] {
        for (text, denied) in [("ordinary input", false), ("secret-", true)] {
            // Matching across fragments uses the same newline boundary in both adapters.
            let mut body = request(router);
            body["messages"] = json!([
                {"role": "user", "content": text}, {"role": "user", "content": "1234"}
            ]);
            let id = format!("{router}-{denied}");
            let response = server
                .post("/v1/chat/completions")
                .add_header("x-bitrouter-request-id", id.clone())
                .json(&body)
                .await;
            ensure!(
                response.status_code().is_success() != denied,
                "{}",
                response.text()
            );
        }
    }
    ensure!(
        upstream
            .received_requests()
            .await
            .context("upstream capture")?
            .len()
            == 2
    );
    Ok(())
}

#[tokio::test]
async fn unified_entry_preserves_router_binding_order_and_leaves_unbound_checks_inert() -> Result<()>
{
    use bitrouter_sdk::config::router::RouterRequestCheck;
    use bitrouter_sdk::extension::request_check::{
        Callback as CheckCallback, Decision as CheckDecision,
    };
    use std::sync::{Arc, mpsc};

    let upstream = MockServer::start().await;
    mount_upstream(&upstream).await;
    let mut config = configuration(&upstream.uri(), 5_000)?;
    for id in ["first", "second", "unbound"] {
        native_config(&mut config, id, "rules-v1");
    }
    let coding = config
        .routers
        .get_mut("coding")
        .context("missing coding router")?;
    coding.checks.request.insert(
        0,
        RouterRequestCheck {
            checker: "second".to_owned(),
            timeout_ms: 5_000,
            max_input_bytes: config::router::DEFAULT_CHECKER_MAX_INPUT_BYTES,
        },
    );
    // Repeated bindings remain ordered invocations.
    let repeated = coding
        .checks
        .request
        .last()
        .context("missing coding binding")?
        .clone();
    coding.checks.request.push(repeated);
    let restricted = config
        .routers
        .get_mut("restricted")
        .context("missing restricted router")?;
    restricted
        .checks
        .request
        .first_mut()
        .context("missing restricted checker binding")?
        .checker = "first".to_owned();

    let (calls_tx, calls_rx) = mpsc::channel();
    let callback = |name: &'static str| -> Arc<CheckCallback> {
        let calls = calls_tx.clone();
        Arc::new(move |_| {
            let _ = calls.send(name);
            CheckDecision::Allow
        })
    };
    let first = callback("first");
    let unbound = callback("unbound");
    let second = callback("second");
    let assembled = bitrouter::assemble::build_app_with_extensions(&config, None, |api| {
        // Registration order differs from the coding router's declared order.
        api.request_check("first", "rules-v1", first)?;
        api.request_check("unbound", "rules-v1", unbound)?;
        Ok(api.request_check("second", "rules-v1", second)?)
    })
    .await?;
    let server = gateway(&assembled)?;
    for router in ["coding", "restricted"] {
        let response = server
            .post("/v1/chat/completions")
            .json(&request(router))
            .await;
        ensure!(response.status_code().is_success(), "{}", response.text());
    }
    let calls = calls_rx.try_iter().collect::<Vec<_>>();
    ensure!(calls == ["second", "first", "first", "first"], "{calls:?}");
    Ok(())
}

#[tokio::test]
async fn undeclared_registrations_stay_inactive_with_sorted_startup_diagnostics() -> Result<()> {
    let upstream = MockServer::start().await;
    mount_upstream(&upstream).await;
    let mut config = configuration(&upstream.uri(), 5_000)?;
    config.checkers.remove("second");
    config.routers.remove("restricted");
    let capture = Capture::default();
    let assembled = bitrouter::assemble::build_app_with_extensions(&config, None, |api| {
        api.request_check("second", "rules-v1", capture.callback("second"))?;
        api.request_check("first", "rules-v1", capture.callback("first"))?;
        Ok(api.request_check("alpha", "rules-v1", capture.callback("alpha"))?)
    })
    .await?;
    let diagnostics = assembled
        .ignored_config
        .iter()
        .filter(|message| message.starts_with("request-check registration "))
        .map(String::as_str)
        .collect::<Vec<_>>();
    ensure!(
        diagnostics
            == [
                "request-check registration 'alpha' is inactive: no checkers.alpha declaration",
                "request-check registration 'second' is inactive: no checkers.second declaration",
            ]
    );
    ensure!(
        capture
            .received_requests()
            .await
            .context("callback capture")?
            .is_empty()
    );

    let response = gateway(&assembled)?
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "configured-only")
        .json(&request("coding"))
        .await;
    ensure!(response.status_code().is_success(), "{}", response.text());
    let calls = capture
        .received_requests()
        .await
        .context("callback capture")?;
    ensure!(calls.len() == 1 && calls[0].id == "first");
    ensure!(
        upstream
            .received_requests()
            .await
            .context("upstream capture")?
            .len()
            == 1
    );
    Ok(())
}

#[tokio::test]
async fn unified_entry_registration_failure_precedes_database_startup() -> Result<()> {
    use bitrouter_sdk::extension::request_check::Decision as CheckDecision;
    use std::sync::Arc;

    let directory = tempfile::tempdir()?;
    let database_path = directory.path().join("must-not-exist.db");
    let mut config = configuration("https://unused-upstream.invalid", 5_000)?;
    config.database.url = format!("sqlite://{}?mode=rwc", database_path.display());

    let error = match bitrouter::assemble::build_app_with_extensions(&config, None, |_| {
        anyhow::bail!("fixture registration failed")
    })
    .await
    {
        Err(error) => error,
        Ok(_) => anyhow::bail!("registration failure unexpectedly activated the host"),
    };
    let error_chain = format!("{error:#}");
    ensure!(error_chain.contains("registering extensions"));
    ensure!(error_chain.contains("fixture registration failed"));
    ensure!(
        !database_path.exists(),
        "database opened before registration completed"
    );

    let duplicate = match bitrouter::assemble::build_app_with_extensions(&config, None, |api| {
        api.request_check("duplicate", "rules-v1", Arc::new(|_| CheckDecision::Allow))?;
        // Ignoring a registration error must still abort assembly before DB I/O.
        let _ = api.request_check("duplicate", "rules-v1", Arc::new(|_| CheckDecision::Allow));
        Ok(())
    })
    .await
    {
        Err(error) => error,
        Ok(_) => anyhow::bail!("duplicate registration unexpectedly activated the host"),
    };
    ensure!(format!("{duplicate:#}").contains("already registered"));
    ensure!(
        !database_path.exists(),
        "database opened after duplicate registration"
    );
    for (id, revision, diagnostic) in [
        ("invalid.id", "rules-v1", "invalid checker id"),
        ("unused", "invalid:revision", "native revision is invalid"),
    ] {
        let invalid = bitrouter::assemble::build_app_with_extensions(&config, None, |api| {
            // These instances are absent from configuration. Ignoring their
            // registration errors must still invalidate the whole registry.
            let _ = api.request_check(id, revision, Arc::new(|_| CheckDecision::Allow));
            Ok(())
        })
        .await;
        let error = match invalid {
            Err(error) => error,
            Ok(_) => anyhow::bail!("invalid unused registration activated the host"),
        };
        ensure!(format!("{error:#}").contains(diagnostic), "{error:#}");
        ensure!(
            !database_path.exists(),
            "database opened after invalid registration"
        );
    }
    Ok(())
}

#[tokio::test]
async fn unified_entry_binding_failures_precede_database_startup() -> Result<()> {
    use bitrouter_sdk::extension::request_check::Decision as CheckDecision;
    use std::sync::Arc;

    for (case, diagnostic, bound) in [
        ("missing", "is not registered", true),
        ("missing", "is not registered", false),
        ("mismatched", "revision does not match", true),
        ("mismatched", "revision does not match", false),
    ] {
        let directory = tempfile::tempdir()?;
        let database_path = directory.path().join("must-not-exist.db");
        let mut config = configuration("https://unused-upstream.invalid", 5_000)?;
        native_config(&mut config, "first", "rules-v1");
        if !bound {
            for router in config.routers.values_mut() {
                router.checks.request.clear();
            }
        }
        config.database.url = format!("sqlite://{}?mode=rwc", database_path.display());
        let result = bitrouter::assemble::build_app_with_extensions(&config, None, |api| {
            api.request_check("second", "rules-v1", Arc::new(|_| CheckDecision::Allow))?;
            if case != "missing" {
                let revision = if case == "mismatched" {
                    "rules-v2"
                } else {
                    "rules-v1"
                };
                api.request_check("first", revision, Arc::new(|_| CheckDecision::Allow))?;
            }
            Ok(())
        })
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => anyhow::bail!("{case} registration activated the host"),
        };
        ensure!(
            format!("{error:#}").contains(diagnostic),
            "{case}: {error:#}"
        );
        ensure!(
            !database_path.exists(),
            "{case}: database opened before binding validation"
        );
    }
    Ok(())
}

#[tokio::test]
async fn native_failures_and_unbound_registration_never_dispatch_unintended_work() -> Result<()> {
    use bitrouter::request_checks::RequestCheckRuntime;
    use bitrouter_sdk::extension::request_check::{
        Callback as CheckCallback, Decision as CheckDecision,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let upstream = MockServer::start().await;
    let mut config = configuration(&upstream.uri(), 20)?;
    // Invalid-result validation is independent of the timeout scenario. Give
    // its worker enough scheduling time under a loaded workspace test run.
    config
        .routers
        .get_mut("restricted")
        .context("restricted router")?
        .checks
        .request[0]
        .timeout_ms = 5_000;
    for id in ["first", "second"] {
        native_config(&mut config, id, "rules-v1");
    }
    ensure!(RequestCheckRuntime::activate(&config).is_err());
    let malformed: Arc<CheckCallback> = Arc::new(|_| CheckDecision::Deny {
        reason_code: "invalid reason".to_owned(),
    });
    let slow: Arc<CheckCallback> = Arc::new(|_| {
        std::thread::sleep(Duration::from_millis(150));
        CheckDecision::Allow
    });
    let assembled = bitrouter::assemble::build_app_with_extensions(&config, None, |api| {
        api.request_check("first", "rules-v1", slow)?;
        Ok(api.request_check("second", "rules-v1", malformed)?)
    })
    .await?;
    let server = gateway(&assembled)?;
    for router in ["coding", "restricted"] {
        for stream in [false, true] {
            let id = format!("native-{router}-{stream}");
            let mut body = request(router);
            body["stream"] = Value::Bool(stream);
            let response = server
                .post("/v1/chat/completions")
                .add_header("x-bitrouter-request-id", id.clone())
                .json(&body)
                .await;
            ensure!(!response.status_code().is_success());
        }
    }
    ensure!(
        upstream
            .received_requests()
            .await
            .context("upstream capture")?
            .is_empty()
    );
    // Merely registering a configured instance does not install a global hook.
    config
        .routers
        .get_mut("coding")
        .context("coding")?
        .checks
        .request
        .clear();
    config.checkers.remove("second");
    config.routers.remove("restricted");
    let calls = Arc::new(AtomicUsize::new(0));
    let captured = calls.clone();
    mount_upstream(&upstream).await;
    let assembled = bitrouter::assemble::build_app_with_extensions(&config, None, |api| {
        Ok(api.request_check(
            "first",
            "rules-v1",
            Arc::new(move |_| {
                captured.fetch_add(1, Ordering::SeqCst);
                CheckDecision::Deny {
                    reason_code: "unused".to_owned(),
                }
            }),
        )?)
    })
    .await?;
    let server = gateway(&assembled)?;
    ensure!(
        server
            .post("/v1/chat/completions")
            .json(&request("coding"))
            .await
            .status_code()
            .is_success()
    );
    ensure!(calls.load(Ordering::SeqCst) == 0);
    Ok(())
}
