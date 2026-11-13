//! Actual standalone CLI against a local Rust metrics fixture, without a daemon.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use tokio::process::Command;

use bitrouter::connector::config::ConnectorConfig;

const WAIT: Duration = Duration::from_secs(15);
const PRIVATE_RESPONSE: &str = "fixture-private-upstream-content";
const TEST_BEARER: &str = "fixture-bearer-not-a-real-secret";

#[derive(Clone)]
struct FixtureState {
    hits: Arc<Mutex<Vec<(String, String)>>>,
}

struct MetricsFixture {
    address: SocketAddr,
    state: FixtureState,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl MetricsFixture {
    async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let state = FixtureState {
            hits: Arc::new(Mutex::new(Vec::new())),
        };
        let router = Router::new()
            .fallback(fixture_response)
            .with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await });
        Ok(Self {
            address,
            state,
            task,
        })
    }

    fn hits(&self) -> Result<Vec<(String, String)>> {
        self.state
            .hits
            .lock()
            .map(|hits| hits.clone())
            .map_err(|_| anyhow::anyhow!("fixture request log poisoned"))
    }
}

impl Drop for MetricsFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture_response(State(state): State<FixtureState>, request: Request) -> Response {
    let path = request.uri().path().to_string();
    if let Ok(mut hits) = state.hits.lock() {
        hits.push((request.method().to_string(), path.clone()));
    } else {
        return (StatusCode::INTERNAL_SERVER_ERROR, String::new()).into_response();
    }
    if matches!(
        path.as_str(),
        "/metrics/authenticated" | "/metrics/authenticated-redirect"
    ) {
        let expected = format!("Bearer {TEST_BEARER}");
        if request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            != Some(expected.as_str())
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        return if path == "/metrics/authenticated" {
            (StatusCode::SERVICE_UNAVAILABLE, PRIVATE_RESPONSE).into_response()
        } else {
            (
                StatusCode::FOUND,
                [("location", "/primary/v1/chat/completions")],
                PRIVATE_RESPONSE,
            )
                .into_response()
        };
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let model = if path == "/metrics/wrong-model" {
        "different-model"
    } else {
        "fixture-model"
    };
    let timestamp = if path == "/metrics/stale" { 1 } else { now };
    let mut body = format!(
        "vllm:num_requests_running{{model_name=\"{model}\",engine=\"0\"}} 2 {timestamp}\n\
         vllm:num_requests_waiting{{model_name=\"{model}\",engine=\"0\"}} 1 {timestamp}\n"
    );
    if path != "/metrics/incomplete" {
        body.push_str(&format!(
            "vllm:kv_cache_usage_perc{{model_name=\"{model}\",engine=\"0\"}} 0.25 {timestamp}\n"
        ));
    }
    match (request.method().as_str(), path.as_str()) {
        (
            "GET",
            "/metrics/healthy" | "/metrics/wrong-model" | "/metrics/stale" | "/metrics/incomplete",
        ) => (StatusCode::OK, body),
        ("GET", "/metrics/unavailable") => (
            StatusCode::SERVICE_UNAVAILABLE,
            PRIVATE_RESPONSE.to_string(),
        ),
        ("GET", "/metrics/oversize") => (StatusCode::OK, "x".repeat(2_048)),
        ("POST", "/primary/v1/chat/completions") => (
            StatusCode::OK,
            r#"{"choices":[{"message":{"content":"primary remains reachable"}}]}"#.to_string(),
        ),
        _ => (StatusCode::NOT_FOUND, String::new()),
    }
    .into_response()
}

fn config(address: SocketAddr, path: &str) -> String {
    format!(
        "version: 1\npool:\n  id: cli-pilot\n  model: fixture-model\n  scope:\n    kind: static_members\n  members:\n    - id: replica-a\n      inference_endpoint: http://{address}/primary/v1\n      metrics_endpoint: http://{address}{path}\n      engines: [\"0\"]\nobservation:\n  timeout_ms: 500\n  max_body_bytes: 1024\n  max_sample_age_ms: 15000\n"
    )
}

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
    command
        .current_dir(home)
        .env("BITROUTER_HOME", home.join("ambient"))
        .env("RUST_LOG", "error")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .kill_on_drop(true);
    command
}

async fn invoke(home: &Path, args: &[&str]) -> Result<std::process::Output> {
    Ok(tokio::time::timeout(WAIT, command(home).args(args).output()).await??)
}

fn parsed(output: &std::process::Output) -> Result<Value> {
    for bytes in [&output.stdout, &output.stderr] {
        ensure!(!String::from_utf8_lossy(bytes).contains(PRIVATE_RESPONSE));
        ensure!(!String::from_utf8_lossy(bytes).contains(TEST_BEARER));
    }
    serde_json::from_slice(&output.stdout).context("connector command did not emit one JSON value")
}

fn assert_observation_only(report: &Value) -> Result<()> {
    let capabilities = &report["capabilities"];
    ensure!(capabilities["adapter"] == "vllm_prometheus_v1");
    for capability in [
        "inference_dispatch",
        "capacity_estimation",
        "admission_control",
        "budget_actuation",
        "durable_execution",
        "pool_discovery",
        "router_binding_verification",
    ] {
        ensure!(
            capabilities[capability] == false,
            "unexpected capability {capability}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn standalone_cli_checks_offline_and_observes_without_gateway_side_effects() -> Result<()> {
    let fixture = MetricsFixture::start().await?;
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    std::fs::create_dir(home.join("ambient"))?;
    // Any accidental gateway or context discovery must fail instead of hiding
    // behind a valid ambient configuration.
    std::fs::write(home.join("ambient/bitrouter.yaml"), "not: [valid YAML")?;
    std::fs::write(home.join("ambient/contexts.toml"), "not = [valid TOML")?;
    let path = home.join("connector.yaml");
    std::fs::write(&path, config(fixture.address, "/metrics/healthy"))?;

    let check = invoke(home, &["connector", "check", "--config", "connector.yaml"]).await?;
    let check_report = parsed(&check)?;
    ensure!(
        check.status.success(),
        "offline check failed: {check_report}"
    );
    assert_observation_only(&check_report)?;
    ensure!(check_report["validation"] == "configuration_only");
    ensure!(check_report["network_requests"] == 0);
    ensure!(fixture.hits()?.is_empty(), "check contacted the network");

    let live = invoke(
        home,
        &[
            "connector",
            "observe",
            "--config",
            "connector.yaml",
            "--json",
        ],
    )
    .await?;
    let snapshot = parsed(&live)?;
    ensure!(live.status.success(), "healthy snapshot failed: {snapshot}");
    ensure!(snapshot["pool_id"] == "cli-pilot" && snapshot["model"] == "fixture-model");
    ensure!(snapshot["summary"]["observed_members"] == 1);
    ensure!(snapshot["summary"]["metrics"]["running_requests"] == 2);
    ensure!(snapshot["summary"]["metrics"]["waiting_requests"] == 1);
    ensure!(snapshot["summary"]["metrics"]["kv_cache_usage_fraction_max"] == 0.25);
    ensure!(snapshot["members"][0]["status"] == "observed");
    ensure!(snapshot["members"][0]["freshness"]["source"] == "explicit_timestamps");
    ensure!(snapshot["scope_evidence"]["coverage"] == "operator_declared_static_members");
    for claim in [
        "physical_membership_verified",
        "router_binding_verified",
        "inference_health_verified",
    ] {
        ensure!(snapshot["scope_evidence"][claim] == false);
    }
    assert_observation_only(&snapshot)?;
    ensure!(fixture.hits()? == vec![("GET".into(), "/metrics/healthy".into())]);

    let human = invoke(
        home,
        &[
            "connector",
            "check",
            "--config",
            "connector.yaml",
            "--human",
        ],
    )
    .await?;
    ensure!(human.status.success());
    ensure!(String::from_utf8_lossy(&human.stdout).contains("cli-pilot"));
    ensure!(serde_json::from_slice::<Value>(&human.stdout).is_err());

    let human_live = invoke(
        home,
        &[
            "connector",
            "observe",
            "--config",
            "connector.yaml",
            "--human",
        ],
    )
    .await?;
    ensure!(human_live.status.success());
    let text = String::from_utf8_lossy(&human_live.stdout);
    ensure!(text.contains("replica-a: observed") && text.contains("source timestamps"));
    ensure!(text.contains("unverified") && text.contains("not idle capacity"));
    ensure!(serde_json::from_slice::<Value>(&human_live.stdout).is_err());

    // Remote refusal happens before reading even a nonexistent input or the
    // intentionally invalid named-context file.
    let remote = invoke(
        home,
        &[
            "--context",
            "remote-pilot",
            "connector",
            "observe",
            "--config",
            "missing.yaml",
        ],
    )
    .await?;
    let refusal = parsed(&remote)?;
    ensure!(!remote.status.success());
    ensure!(
        refusal["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("local-only"))
    );
    ensure!(fixture.hits()?.len() == 2);

    std::fs::write(&path, "x".repeat(65_537))?;
    let too_large = invoke(home, &["connector", "check", "--config", "connector.yaml"]).await?;
    let oversized = parsed(&too_large)?;
    ensure!(!too_large.status.success());
    ensure!(
        oversized["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("64 KiB"))
    );
    ensure!(fixture.hits()?.len() == 2);

    let ambient: Vec<_> = std::fs::read_dir(home.join("ambient"))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<_>>()?;
    ensure!(ambient.len() == 2, "connector created gateway state");
    let root_entries = std::fs::read_dir(home)?.collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        root_entries.len() == 2,
        "connector created local runtime state"
    );
    Ok(())
}

#[tokio::test]
async fn missing_stale_incomplete_or_wrong_model_metrics_fail_without_touching_primary()
-> Result<()> {
    let fixture = MetricsFixture::start().await?;
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    let paths = [
        "/metrics/unavailable",
        "/metrics/stale",
        "/metrics/incomplete",
        "/metrics/wrong-model",
        "/metrics/oversize",
    ];
    for (path, reason, status) in [
        (paths[0], "http_status", "unknown"),
        (paths[1], "stale_sample", "stale"),
        (paths[2], "missing_metric", "unknown"),
        (paths[3], "scope_mismatch", "unknown"),
        (paths[4], "body_too_large", "unknown"),
    ] {
        std::fs::write(home.join("connector.yaml"), config(fixture.address, path))?;
        let output = invoke(
            home,
            &["connector", "observe", "--config", "connector.yaml"],
        )
        .await?;
        let snapshot = parsed(&output)?;
        ensure!(
            !output.status.success(),
            "bad metrics accepted: {path}: {snapshot}"
        );
        ensure!(snapshot["pool_id"] == "cli-pilot");
        ensure!(
            snapshot["members"]
                .as_array()
                .is_some_and(|members| members.len() == 1)
        );
        ensure!(snapshot["members"][0]["status"] == status);
        ensure!(snapshot["members"][0]["reason"]["code"] == reason);
        ensure!(snapshot["members"][0]["metrics"].is_null());
        ensure!(snapshot["summary"]["metrics"].is_null());
        assert_observation_only(&snapshot)?;
    }
    ensure!(fixture.hits()? == paths.map(|path| ("GET".into(), path.into())));

    std::fs::write(
        home.join("connector.yaml"),
        config(fixture.address, "/metrics/incomplete"),
    )?;
    let human = invoke(
        home,
        &[
            "connector",
            "observe",
            "--config",
            "connector.yaml",
            "--human",
        ],
    )
    .await?;
    ensure!(!human.status.success());
    let text = String::from_utf8_lossy(&human.stdout);
    ensure!(text.contains("replica-a: unknown") && text.contains("totals unavailable"));
    ensure!(!text.contains(PRIVATE_RESPONSE));

    let primary = reqwest::Client::builder()
        .no_proxy()
        .build()?
        .post(format!(
            "http://{}/primary/v1/chat/completions",
            fixture.address
        ))
        .json(&serde_json::json!({"model":"fixture-model","messages":[]}))
        .send()
        .await?;
    ensure!(primary.status().is_success());
    ensure!(primary.text().await?.contains("primary remains reachable"));
    ensure!(
        fixture.hits()?.last() == Some(&("POST".into(), "/primary/v1/chat/completions".into()))
    );
    Ok(())
}

#[tokio::test]
async fn metrics_bearer_is_sent_only_to_selected_endpoint_and_never_printed() -> Result<()> {
    let fixture = MetricsFixture::start().await?;
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    for (path, reason) in [
        ("/metrics/authenticated", "http_status"),
        ("/metrics/authenticated-redirect", "redirect"),
    ] {
        let source = config(fixture.address, path).replace(
            "engines: [\"0\"]",
            "engines: [\"0\"]\n      metrics_bearer_env: CONNECTOR_TEST_BEARER",
        );
        std::fs::write(home.join("connector.yaml"), source)?;
        let check = tokio::time::timeout(
            WAIT,
            command(home)
                .env_remove("CONNECTOR_TEST_BEARER")
                .args(["connector", "check", "--config", "connector.yaml"])
                .output(),
        )
        .await??;
        let plan = parsed(&check)?;
        ensure!(check.status.success());
        ensure!(plan["members"][0]["metrics_authentication"] == "environment_bearer");
        ensure!(plan["network_requests"] == 0);
        let output = tokio::time::timeout(
            WAIT,
            command(home)
                .env("CONNECTOR_TEST_BEARER", TEST_BEARER)
                .args(["connector", "observe", "--config", "connector.yaml"])
                .output(),
        )
        .await??;
        let snapshot = parsed(&output)?;
        ensure!(!output.status.success());
        ensure!(snapshot["members"][0]["reason"]["code"] == reason);
        if reason == "http_status" {
            ensure!(snapshot["members"][0]["reason"]["status"] == 503);
        }
    }
    ensure!(
        fixture.hits()?
            == vec![
                ("GET".into(), "/metrics/authenticated".into()),
                ("GET".into(), "/metrics/authenticated-redirect".into()),
            ]
    );
    Ok(())
}

#[test]
fn standalone_configuration_schema_and_example_remain_in_lockstep() -> Result<()> {
    let generated = serde_json::to_value(schemars::schema_for!(ConnectorConfig))?;
    let committed: Value = serde_json::from_str(include_str!(
        "../../../docs/specs/configuration/provider-connector-v1.schema.json"
    ))?;
    ensure!(
        generated == committed,
        "regenerate the connector configuration schema"
    );
    let example =
        include_str!("../../../docs/specs/configuration/provider-connector-v1.example.yaml");
    let config = ConnectorConfig::from_yaml(example)?;
    config.validate()?;
    ensure!(config.version == 1 && config.pool.members.len() == 1);
    ensure!(config.observation.timeout_ms == 2000);
    ensure!(
        config
            .execution
            .as_ref()
            .is_some_and(|execution| !execution.enabled)
    );
    let observation_only = example
        .split("# Optional pilot execution.")
        .next()
        .context("observation compatibility fixture")?;
    ensure!(
        ConnectorConfig::from_yaml(observation_only)?
            .execution
            .is_none()
    );
    ensure!(
        ConnectorConfig::from_yaml(&format!("{observation_only}\nexecution: null\n"))?
            .execution
            .is_none()
    );
    let omitted_acknowledgement = example.replace("  allow_collection_only_metrics: false\n", "");
    ensure!(
        ConnectorConfig::from_yaml(&omitted_acknowledgement)?
            .execution
            .as_ref()
            .is_some_and(|execution| !execution.allow_collection_only_metrics)
    );
    for incompatible in [
        example.replace("version: 1", "version: 2"),
        example.replace("kind: static_members", "kind: discovered_router"),
        example.replace(
            "kind: static_members",
            "kind: static_members\n    unknown: true",
        ),
        format!("{example}\nunknown: true\n"),
        example.replace("timeout_ms: 2000", "timeout_ms: null"),
        example.replace("  max_extra_inflight: 1\n", ""),
        example.replace("  max_output_tokens: 128", "  max_output_tokens: null"),
        example.replace("  burst: 1", "  burst: 0"),
        example.replace("  enabled: false", "  enabled: false\n  unknown: true"),
        example.replace(
            "  exclusive_pool_owner: true",
            "  exclusive_pool_owner: false",
        ),
        example.replace(
            "  state_dir: /var/lib/bitrouter/connector-local-pilot",
            "  state_dir: relative-state",
        ),
        example.replace("engines: [\"0\"]", "engines: []"),
        example.replace("engines: [\"0\"]", "engines: [\"0\", \"0\"]"),
        example.replace(
            "http://127.0.0.1:8000/v1",
            "http://name:credential@127.0.0.1:8000/v1",
        ),
    ] {
        ensure!(ConnectorConfig::from_yaml(&incompatible).is_err());
    }
    Ok(())
}
