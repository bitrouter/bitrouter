use std::path::{Path, PathBuf};

use axum_test::TestServer;
use bitrouter::workflow_state::decision::{POLICY_DECISION_JSONL_ENV, PolicyDecisionRecord};
use bitrouter::workflow_state::ir::HarnessId;
use bitrouter::workflow_state::real_trace::{RealTraceCapture, TraceCaptureOptions};
use bitrouter_sdk::config::{self, resolve_presets};
use bitrouter_sdk::server::{AppState, RouterOptions, build_router_with_options};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PRIVATE_IDENTITY_HEADERS: &[&str] = &[
    "x-bitrouter-harness",
    "x-bitrouter-workflow-",
    "x-bitrouter-agent-",
    "x-superpowers-",
];

static DECISION_RECORDER_ENV_LOCK: Mutex<()> = Mutex::const_new(());

struct NativeCase {
    name: &'static str,
    source: HarnessId,
    path: &'static str,
    headers: &'static [(&'static str, &'static str)],
    body: Value,
}

#[tokio::test]
async fn native_http_matrix_routes_without_private_workflow_headers() {
    let _env_lock = DECISION_RECORDER_ENV_LOCK.lock().await;
    let upstream = mock_chat_upstream().await;
    let temp = TempDir::new().expect("temporary decision directory");
    let decisions_path = temp.path().join("decisions.jsonl");
    let _decision_env = DecisionRecorderEnv::set(&decisions_path);
    let (server, capture) = generalization_server(&upstream.uri()).await;

    for case in native_cases() {
        let mut request = server.post(case.path);
        for (name, value) in case.headers {
            request = request.add_header(*name, *value);
        }
        let response = request.json(&case.body).await;
        response.assert_status_ok();
    }

    let traces = capture.records();
    assert_eq!(traces.len(), 7, "every native request must be captured");
    for (trace, case) in traces.iter().zip(native_cases()) {
        assert_eq!(trace.harness, case.source, "{} native evidence", case.name);
        assert!(
            trace
                .headers
                .keys()
                .all(|header| !is_private_identity_header(header)),
            "{} must not smuggle private identity headers: {:?}",
            case.name,
            trace.headers
        );
    }

    let decisions = PolicyDecisionRecord::load_jsonl(&decisions_path)
        .expect("HTTP traffic emits readable policy decisions");
    assert_eq!(decisions.len(), 7, "each HTTP request emits one decision");
    for decision in &decisions {
        assert_eq!(decision.key_strategy, "agent_trace");
        assert_eq!(decision.request_key, "agent_trace/v1|opening|normal");
        assert_eq!(
            decision.selected_tier.as_deref(),
            Some("strong"),
            "unmatched opening traces use the template's strong default: {decision:?}"
        );
        assert_eq!(
            decision.selected_model.as_deref(),
            Some("openai-codex:gpt-5.6-sol")
        );
        let serialized = serde_json::to_value(decision).expect("decision serializes");
        assert!(
            serialized.get("harness_id").is_none() && serialized.get("source").is_none(),
            "runtime source stays in the captured diagnostic trace, not decision policy fields: {serialized}"
        );
    }
}

#[tokio::test]
async fn auto_template_keeps_normal_traces_shared_and_guarded_traces_strong() {
    let _env_lock = DECISION_RECORDER_ENV_LOCK.lock().await;
    let template = template_config();
    for model in ["@auto", "@auto:cost"] {
        let resolution = resolve_presets(model, &template.presets, &template.variants)
            .expect("template auto preset resolves");
        assert_eq!(resolution.policy.as_deref(), Some("auto"));
        assert_eq!(resolution.clean_model, "openai-codex:gpt-5.6-sol");
    }

    let upstream = mock_chat_upstream().await;
    let temp = TempDir::new().expect("temporary decision directory");
    let decisions_path = temp.path().join("decisions.jsonl");
    let _decision_env = DecisionRecorderEnv::set(&decisions_path);
    let (server, _) = generalization_server(&upstream.uri()).await;

    for (path, body) in [
        (
            "/v1/chat/completions",
            json!({
                "model": "@auto",
                "messages": [
                    {"role": "user", "content": "continue"},
                    {"role": "assistant", "tool_calls": [{
                        "id": "call_edit", "type": "function",
                        "function": {"name": "apply_patch", "arguments": "{}"}
                    }]}
                ]
            }),
        ),
        (
            "/v1/chat/completions",
            json!({
                "model": "@auto:cost",
                "messages": [
                    {"role": "user", "content": "continue"},
                    {"role": "assistant", "tool_calls": [{
                        "id": "call_test", "type": "function",
                        "function": {"name": "cargo_test", "arguments": "{}"}
                    }]}
                ]
            }),
        ),
        (
            "/v1/chat/completions",
            json!({
                "model": "@auto",
                "messages": [
                    {"role": "user", "content": "continue"},
                    {"role": "assistant", "tool_calls": [{
                        "id": "call_bash", "type": "function",
                        "function": {"name": "bash", "arguments": "{}"}
                    }]}
                ]
            }),
        ),
    ] {
        server.post(path).json(&body).await.assert_status_ok();
    }

    server
        .post("/v1/chat/completions")
        .json(&json!({
            "model": "@auto",
            "messages": [
                {"role": "user", "content": "continue"},
                {"role": "assistant", "tool_calls": [{
                    "id": "call_failed", "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}
                }]},
                {"role": "tool", "tool_call_id": "call_failed", "content": "error: cargo test failed"}
            ]
        }))
        .await
        .assert_status_ok();

    let decisions = PolicyDecisionRecord::load_jsonl(&decisions_path)
        .expect("HTTP traffic emits readable policy decisions");
    assert_eq!(decisions.len(), 4);
    for decision in &decisions[..3] {
        assert_eq!(decision.request_key, "agent_trace/v1|tool_followup|normal");
        assert_eq!(decision.selected_tier.as_deref(), Some("economy"));
    }
    assert_eq!(decisions[3].request_key, "agent_trace/v1|recovery|guarded");
    assert_eq!(decisions[3].selected_tier.as_deref(), Some("strong"));
    assert_eq!(
        decisions[3].selected_model.as_deref(),
        Some("openai-codex:gpt-5.6-sol")
    );
}

#[tokio::test]
async fn native_sources_share_template_projection_keys_and_tiers() {
    let _env_lock = DECISION_RECORDER_ENV_LOCK.lock().await;
    let upstream = mock_chat_upstream().await;
    let temp = TempDir::new().expect("temporary decision directory");
    let decisions_path = temp.path().join("decisions.jsonl");
    let _decision_env = DecisionRecorderEnv::set(&decisions_path);
    let (server, capture) = generalization_server(&upstream.uri()).await;

    let scenarios = [
        (
            "edit",
            "agent_trace/v1|edit|normal",
            terminus_action_case("@auto", "apply_patch <<'PATCH'\nPATCH"),
            openclaw_action_case("@auto:cost", "edit"),
            "economy",
            "bitrouter:deepseek/deepseek-v4-pro",
        ),
        (
            "test",
            "agent_trace/v1|test|normal",
            terminus_action_case("@auto", "cargo test -p bitrouter"),
            openclaw_action_case("@auto:cost", "test"),
            "economy",
            "bitrouter:deepseek/deepseek-v4-pro",
        ),
        (
            "tool followup",
            "agent_trace/v1|tool_followup|normal",
            native_tool_followup_case("@auto", HarnessId::Terminus2),
            native_tool_followup_case("@auto:cost", HarnessId::OpenClaw),
            "economy",
            "bitrouter:deepseek/deepseek-v4-pro",
        ),
        (
            "recovery",
            "agent_trace/v1|recovery|guarded",
            native_recovery_case("@auto", HarnessId::Terminus2),
            native_recovery_case("@auto:cost", HarnessId::OpenClaw),
            "strong",
            "openai-codex:gpt-5.6-sol",
        ),
    ];

    for (_, _, first, second, _, _) in &scenarios {
        post_native_case(&server, first).await;
        post_native_case(&server, second).await;
    }

    let traces = capture.records();
    assert_eq!(traces.len(), 8);
    for pair in traces.chunks_exact(2) {
        assert_eq!(pair[0].harness, HarnessId::Terminus2);
        assert_eq!(pair[1].harness, HarnessId::OpenClaw);
        assert!(pair.iter().all(|trace| {
            trace
                .headers
                .keys()
                .all(|header| !is_private_identity_header(header))
        }));
    }

    let decisions = PolicyDecisionRecord::load_jsonl(&decisions_path)
        .expect("native HTTP traffic emits policy decisions");
    assert_eq!(decisions.len(), 8);
    for ((name, key, _, _, tier, model), pair) in scenarios.iter().zip(decisions.chunks_exact(2)) {
        assert_eq!(pair[0].request_key, *key, "{name} first source");
        assert_eq!(pair[1].request_key, *key, "{name} second source");
        assert_eq!(pair[0].selected_tier.as_deref(), Some(*tier), "{name}");
        assert_eq!(pair[1].selected_tier.as_deref(), Some(*tier), "{name}");
        assert_eq!(pair[0].selected_model.as_deref(), Some(*model), "{name}");
        assert_eq!(pair[1].selected_model.as_deref(), Some(*model), "{name}");
    }
}

async fn post_native_case(server: &TestServer, case: &NativeCase) {
    let mut request = server.post(case.path);
    for (name, value) in case.headers {
        request = request.add_header(*name, *value);
    }
    request.json(&case.body).await.assert_status_ok();
}

fn terminus_action_case(model: &str, command: &str) -> NativeCase {
    NativeCase {
        name: "Terminus 2",
        source: HarnessId::Terminus2,
        path: "/v1/chat/completions",
        headers: &[],
        body: json!({
            "model": model,
            "messages": [
                {"role": "system", "content": terminus_contract()},
                {"role": "user", "content": "continue"},
                {"role": "assistant", "content": format!("{{\"commands\":[{{\"keystrokes\":{}}}],\"task_complete\":false}}", serde_json::to_string(command).expect("command serializes"))}
            ]
        }),
    }
}

fn openclaw_action_case(model: &str, action: &str) -> NativeCase {
    NativeCase {
        name: "OpenClaw",
        source: HarnessId::OpenClaw,
        path: "/v1/chat/completions",
        headers: &[],
        body: json!({
            "model": model,
            "agentRuntime": {"id": "openclaw.default"},
            "runtimePlan": {"action": action},
            "messages": [{"role": "user", "content": "continue"}]
        }),
    }
}

fn native_tool_followup_case(model: &str, source: HarnessId) -> NativeCase {
    native_tool_case(model, source, None)
}

fn native_recovery_case(model: &str, source: HarnessId) -> NativeCase {
    native_tool_case(model, source, Some("error: cargo test failed"))
}

fn native_tool_case(model: &str, source: HarnessId, tool_result: Option<&str>) -> NativeCase {
    let mut messages = vec![
        json!({"role": "user", "content": "continue"}),
        json!({"role": "assistant", "tool_calls": [{
            "id": "call_native", "type": "function",
            "function": {"name": "bash", "arguments": "{}"}
        }]}),
    ];
    if let Some(result) = tool_result {
        messages.push(json!({
            "role": "tool", "tool_call_id": "call_native", "content": result
        }));
    }
    match source {
        HarnessId::Terminus2 => {
            messages.insert(0, json!({"role": "system", "content": terminus_contract()}));
            NativeCase {
                name: "Terminus 2",
                source,
                path: "/v1/chat/completions",
                headers: &[],
                body: json!({"model": model, "messages": messages}),
            }
        }
        HarnessId::OpenClaw => NativeCase {
            name: "OpenClaw",
            source,
            path: "/v1/chat/completions",
            headers: &[],
            body: json!({
                "model": model,
                "agentRuntime": {"id": "openclaw.default"},
                "messages": messages
            }),
        },
        _ => unreachable!("only native matrix sources are supported"),
    }
}

fn terminus_contract() -> &'static str {
    "You are an AI assistant tasked with solving command-line tasks in a Linux environment. Format your response as JSON with commands and task_complete."
}

fn native_cases() -> Vec<NativeCase> {
    vec![
        NativeCase {
            name: "Codex",
            source: HarnessId::Codex,
            path: "/v1/responses",
            headers: &[("user-agent", "codex-cli/1.0")],
            body: json!({"model": "@auto", "input": "inspect the repository"}),
        },
        NativeCase {
            name: "Claude Code",
            source: HarnessId::ClaudeCode,
            path: "/v1/messages",
            headers: &[("anthropic-beta", "claude-code-20250219")],
            body: json!({
                "model": "@auto", "max_tokens": 64,
                "messages": [{"role": "user", "content": "inspect the repository"}]
            }),
        },
        NativeCase {
            name: "Hermes",
            source: HarnessId::Hermes,
            path: "/v1/chat/completions",
            headers: &[("user-agent", "hermes-cli/1.0")],
            body: json!({
                "model": "@auto", "metadata": {"job_id": "native-hermes-job"},
                "messages": [{"role": "user", "content": "inspect the repository"}]
            }),
        },
        NativeCase {
            name: "OpenClaw",
            source: HarnessId::OpenClaw,
            path: "/v1/chat/completions",
            headers: &[],
            body: json!({
                "model": "@auto", "agentRuntime": {"id": "openclaw.default"},
                "messages": [{"role": "user", "content": "inspect the repository"}]
            }),
        },
        NativeCase {
            name: "Terminus 2",
            source: HarnessId::Terminus2,
            path: "/v1/chat/completions",
            headers: &[],
            body: json!({
                "model": "@auto",
                "messages": [{"role": "system", "content": "You are an AI assistant tasked with solving command-line tasks in a Linux environment. Format your response as JSON with commands and task_complete."}, {"role": "user", "content": "inspect the repository"}]
            }),
        },
        NativeCase {
            name: "Smithers",
            source: HarnessId::Smithers,
            path: "/v1/chat/completions",
            headers: &[
                ("x-smithers-workflow-id", "native-run"),
                ("x-smithers-node-id", "inspect"),
            ],
            body: json!({
                "model": "@auto",
                "messages": [{"role": "user", "content": "inspect the repository"}]
            }),
        },
        NativeCase {
            name: "Generic",
            source: HarnessId::Generic,
            path: "/v1/chat/completions",
            headers: &[],
            body: json!({
                "model": "@auto",
                "messages": [{"role": "user", "content": "inspect the repository"}]
            }),
        },
    ]
}

async fn generalization_server(upstream: &str) -> (TestServer, RealTraceCapture) {
    let (cfg, config_path) = template_config_with_mock(upstream);
    let assembled = bitrouter::build_app_with_path(&cfg, Some(&config_path))
        .await
        .expect("app assembles");
    let state = AppState {
        language_model: assembled
            .app
            .language_model()
            .expect("language model")
            .clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    };
    let capture = RealTraceCapture::new(TraceCaptureOptions::default());
    let router = build_router_with_options(
        state,
        RouterOptions::default().with_router_wrapper(capture.router_wrapper()),
    );
    (TestServer::new(router), capture)
}

fn template_config() -> config::Config {
    let path = template_root().join("bitrouter.yaml");
    let yaml = std::fs::read_to_string(&path).expect("read auto template");
    config::parse_with(&yaml, |_| None).expect("auto template config parses")
}

fn template_config_with_mock(upstream: &str) -> (config::Config, PathBuf) {
    let config_path = template_root().join("bitrouter.yaml");
    let mut config = template_config();
    config.server.skip_auth = true;
    config.database.url = "sqlite::memory:".to_string();
    for provider_id in ["openai-codex", "bitrouter"] {
        let provider = config
            .providers
            .get_mut(provider_id)
            .expect("template provider exists");
        provider.api_base = upstream.to_string();
        provider.api_key = "test-key".to_string();
    }
    (config, config_path)
}

fn template_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("templates/auto-router")
}

async fn mock_chat_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp-generalization",
            "object": "response",
            "status": "completed",
            "model": "test-model",
            "output": [{
                "id": "msg-generalization",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]
            }],
            "usage": {"input_tokens": 4, "output_tokens": 1, "total_tokens": 5}
        })))
        .mount(&server)
        .await;
    server
}

fn is_private_identity_header(header: &str) -> bool {
    PRIVATE_IDENTITY_HEADERS
        .iter()
        .any(|prefix| header.starts_with(prefix))
}

struct DecisionRecorderEnv(Option<std::ffi::OsString>);

impl DecisionRecorderEnv {
    fn set(path: &Path) -> Self {
        // Tests run this setup before building the app, which reads the recorder path once.
        let previous = std::env::var_os(POLICY_DECISION_JSONL_ENV);
        unsafe { std::env::set_var(POLICY_DECISION_JSONL_ENV, path) };
        Self(previous)
    }
}

impl Drop for DecisionRecorderEnv {
    fn drop(&mut self) {
        if let Some(previous) = &self.0 {
            unsafe { std::env::set_var(POLICY_DECISION_JSONL_ENV, previous) };
        } else {
            unsafe { std::env::remove_var(POLICY_DECISION_JSONL_ENV) };
        }
    }
}
