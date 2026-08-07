use std::path::{Path, PathBuf};

use axum_test::TestServer;
use bitrouter::policy_lock::PolicyLock;
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

const MOCK_STRONG_MODEL: &str = "mock-strong:gpt-5.6-sol";
const MOCK_BALANCED_MODEL: &str = "mock-economy:z-ai/glm-5.2";
const MOCK_ECONOMY_MODEL: &str = "mock-economy:deepseek/deepseek-v4-pro";

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
    let (server, capture, _config_dir) = generalization_server(&upstream.uri()).await;

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
        assert_eq!(decision.request_key, "agent_route/v1|unknown|normal");
        assert_eq!(
            decision.selected_tier.as_deref(),
            Some("strong"),
            "unmatched opening traces use the template's strong default: {decision:?}"
        );
        assert_eq!(decision.selected_model.as_deref(), Some(MOCK_STRONG_MODEL));
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
    let (server, _, _config_dir) = generalization_server(&upstream.uri()).await;

    for (index, (path, mut body)) in [
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
                        "function": {"name": "bash", "arguments": "{\"cmd\":\"cargo test -p bitrouter\"}"}
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
        (
            "/v1/chat/completions",
            json!({
                "model": "@auto:cost",
                "messages": [
                    {"role": "user", "content": "continue"},
                    {"role": "assistant", "tool_calls": [{
                        "id": "call_compound", "type": "function",
                        "function": {"name": "bash", "arguments": "{\"cmd\":\"echo hello && cargo test\"}"}
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
                        "id": "call_carriage_return", "type": "function",
                        "function": {"name": "bash", "arguments": "{\"cmd\":\"cargo test\\r\"}"}
                    }]}
                ]
            }),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let root_text = format!("complete root {index}");
        body["messages"][0]["content"] = serde_json::Value::String(root_text.clone());
        server
            .post(path)
            .json(&json!({
                "model": body["model"].clone(),
                "messages": [{"role": "user", "content": root_text}]
            }))
            .await
            .assert_status_ok();
        server.post(path).json(&body).await.assert_status_ok();
    }

    server
        .post("/v1/chat/completions")
        .json(&json!({
            "model": "@auto",
            "messages": [
                {"role": "system", "content": "You are an AI assistant tasked with solving command-line tasks in a Linux environment. Format your response as JSON with commands and task_complete."},
                {"role": "user", "content": "complete review root"}
            ]
        }))
        .await
        .assert_status_ok();
    server
        .post("/v1/chat/completions")
        .json(&json!({
            "model": "@auto",
            "messages": [
                {"role": "system", "content": "You are an AI assistant tasked with solving command-line tasks in a Linux environment. Format your response as JSON with commands and task_complete."},
                {"role": "user", "content": "complete review root"},
                {"role": "assistant", "content": "{\"commands\":[{\"keystrokes\":\"git diff\"}],\"task_complete\":false}"}
            ]
        }))
        .await
        .assert_status_ok();

    server
        .post("/v1/chat/completions")
        .json(&json!({
            "model": "@auto",
            "messages": [{"role": "user", "content": "complete recovery root"}]
        }))
        .await
        .assert_status_ok();
    server
        .post("/v1/chat/completions")
        .json(&json!({
            "model": "@auto",
            "messages": [
                {"role": "user", "content": "complete recovery root"},
                {"role": "assistant", "tool_calls": [{
                    "id": "call_failed", "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}
                }]},
                {"role": "tool", "tool_call_id": "call_failed", "content": "error: cargo test failed"}
            ]
        }))
        .await
        .assert_status_ok();

    server
        .post("/v1/chat/completions")
        .json(&json!({
            "model": "@auto",
            "messages": [
                {"role": "user", "content": "unseen standalone root"},
                {"role": "assistant", "tool_calls": [{
                    "id": "call_incomplete", "type": "function",
                    "function": {"name": "apply_patch", "arguments": "{}"}
                }]}
            ]
        }))
        .await
        .assert_status_ok();

    let decisions = PolicyDecisionRecord::load_jsonl(&decisions_path)
        .expect("HTTP traffic emits readable policy decisions");
    assert_eq!(decisions.len(), 15);
    for decision in decisions.iter().step_by(2).take(7) {
        assert_eq!(decision.request_key, "agent_route/v1|unknown|normal");
        assert_eq!(decision.selected_tier.as_deref(), Some("strong"));
        assert_eq!(
            decision.trajectory_completeness.as_deref(),
            Some("complete")
        );
    }
    for (decision, key) in decisions[1..10].iter().step_by(2).zip([
        "agent_trace/v2|edit|normal",
        "agent_trace/v2|test|normal",
        "agent_trace/v2|tool_followup|normal",
        "agent_trace/v2|tool_followup|normal",
        "agent_trace/v2|tool_followup|normal",
    ]) {
        assert_eq!(decision.request_key, key);
        assert_eq!(decision.selected_tier.as_deref(), Some("economy"));
        assert_eq!(
            decision.trajectory_completeness.as_deref(),
            Some("complete")
        );
    }
    assert_eq!(decisions[11].request_key, "agent_trace/v2|review|normal");
    assert_eq!(decisions[11].selected_tier.as_deref(), Some("balanced"));
    assert_eq!(
        decisions[11].selected_model.as_deref(),
        Some(MOCK_BALANCED_MODEL)
    );
    assert_eq!(
        decisions[13].request_key,
        "agent_route/v1|implement|guarded"
    );
    assert_eq!(decisions[13].selected_tier.as_deref(), Some("strong"));
    assert_eq!(
        decisions[13].selected_model.as_deref(),
        Some(MOCK_STRONG_MODEL)
    );
    assert_eq!(decisions[14].request_key, "agent_trace/v2|edit|normal");
    assert_eq!(decisions[14].static_tier.as_deref(), Some("economy"));
    assert_eq!(decisions[14].selected_tier.as_deref(), Some("strong"));
    assert_eq!(
        decisions[14].trajectory_completeness.as_deref(),
        Some("incomplete")
    );
}

#[tokio::test]
async fn native_sources_share_template_projection_keys_and_tiers() {
    let _env_lock = DECISION_RECORDER_ENV_LOCK.lock().await;
    let upstream = mock_chat_upstream().await;
    let temp = TempDir::new().expect("temporary decision directory");
    let decisions_path = temp.path().join("decisions.jsonl");
    let _decision_env = DecisionRecorderEnv::set(&decisions_path);
    let (server, capture, _config_dir) = generalization_server(&upstream.uri()).await;

    let mut scenarios = [
        (
            "edit",
            "agent_trace/v2|edit|normal",
            terminus_action_case("@auto", "apply_patch <<'PATCH'\nPATCH"),
            claude_fixture_case("@auto:cost", ClaudeFixture::Edit, None),
            "economy",
            MOCK_ECONOMY_MODEL,
        ),
        (
            "test",
            "agent_trace/v2|test|normal",
            terminus_action_case("@auto", "cargo test -p bitrouter"),
            claude_fixture_case("@auto:cost", ClaudeFixture::Test, None),
            "economy",
            MOCK_ECONOMY_MODEL,
        ),
        (
            "tool followup",
            "agent_trace/v2|tool_followup|normal",
            terminus_tool_case("@auto", None),
            claude_fixture_case("@auto:cost", ClaudeFixture::ToolFollowup, None),
            "economy",
            MOCK_ECONOMY_MODEL,
        ),
        (
            "recovery",
            "agent_route/v1|implement|guarded",
            terminus_tool_case("@auto", Some("error: cargo test failed")),
            claude_fixture_case(
                "@auto:cost",
                ClaudeFixture::ToolFollowup,
                Some("error: cargo test failed"),
            ),
            "strong",
            MOCK_STRONG_MODEL,
        ),
    ];

    for (name, _, first, second, _, _) in &mut scenarios {
        let first_root = complete_native_root(first, &format!("{name} terminus root"));
        post_native_body(&server, first, &first_root).await;
        post_native_case(&server, first).await;
        let second_root = complete_native_root(second, &format!("{name} claude root"));
        post_native_body(&server, second, &second_root).await;
        post_native_case(&server, second).await;
    }

    let traces = capture.records();
    assert_eq!(traces.len(), 16);
    for pair in traces.chunks_exact(4) {
        assert_eq!(pair[0].harness, HarnessId::Terminus2);
        assert_eq!(pair[1].harness, HarnessId::Terminus2);
        assert_eq!(pair[2].harness, HarnessId::ClaudeCode);
        assert_eq!(pair[3].harness, HarnessId::ClaudeCode);
        assert!(pair.iter().all(|trace| {
            trace
                .headers
                .keys()
                .all(|header| !is_private_identity_header(header))
        }));
    }

    let decisions = PolicyDecisionRecord::load_jsonl(&decisions_path)
        .expect("native HTTP traffic emits policy decisions");
    assert_eq!(decisions.len(), 16);
    for ((name, key, _, _, tier, model), pair) in scenarios.iter().zip(decisions.chunks_exact(4)) {
        for root in [&pair[0], &pair[2]] {
            assert_eq!(root.request_key, "agent_route/v1|unknown|normal");
            assert_eq!(root.selected_tier.as_deref(), Some("strong"));
            assert_eq!(root.trajectory_completeness.as_deref(), Some("complete"));
        }
        assert_eq!(pair[1].request_key, *key, "{name} first source");
        assert_eq!(pair[3].request_key, *key, "{name} second source");
        assert_eq!(pair[1].selected_tier.as_deref(), Some(*tier), "{name}");
        assert_eq!(pair[3].selected_tier.as_deref(), Some(*tier), "{name}");
        assert_eq!(pair[1].selected_model.as_deref(), Some(*model), "{name}");
        assert_eq!(pair[3].selected_model.as_deref(), Some(*model), "{name}");
        assert_eq!(pair[1].trajectory_completeness.as_deref(), Some("complete"));
        assert_eq!(pair[3].trajectory_completeness.as_deref(), Some("complete"));
    }
}

async fn post_native_case(server: &TestServer, case: &NativeCase) {
    post_native_body(server, case, &case.body).await;
}

async fn post_native_body(server: &TestServer, case: &NativeCase, body: &Value) {
    let mut request = server.post(case.path);
    for (name, value) in case.headers {
        request = request.add_header(*name, *value);
    }
    request.json(body).await.assert_status_ok();
}

fn complete_native_root(case: &mut NativeCase, root_text: &str) -> Value {
    let user_index = if case.path == "/v1/messages" { 0 } else { 1 };
    let messages = case.body["messages"]
        .as_array_mut()
        .expect("native continuation messages");
    messages[user_index]["content"] = Value::String(root_text.to_owned());
    let mut root = case.body.clone();
    root["messages"]
        .as_array_mut()
        .expect("native root messages")
        .truncate(user_index + 1);
    root
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

fn terminus_tool_case(model: &str, tool_result: Option<&str>) -> NativeCase {
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
    messages.insert(0, json!({"role": "system", "content": terminus_contract()}));
    NativeCase {
        name: "Terminus 2",
        source: HarnessId::Terminus2,
        path: "/v1/chat/completions",
        headers: &[],
        body: json!({"model": model, "messages": messages}),
    }
}

#[derive(Clone, Copy)]
enum ClaudeFixture {
    Edit,
    Test,
    ToolFollowup,
}

fn claude_fixture_case(
    model: &str,
    fixture: ClaudeFixture,
    tool_result: Option<&str>,
) -> NativeCase {
    let text = match fixture {
        ClaudeFixture::Edit => include_str!("fixtures/workflow_state/claude_code/edit_tool.json"),
        ClaudeFixture::Test => include_str!("fixtures/workflow_state/claude_code/test_tool.json"),
        ClaudeFixture::ToolFollowup => {
            include_str!("fixtures/workflow_state/claude_code/tool_followup.json")
        }
    };
    let mut fixture: Value = serde_json::from_str(text).expect("Claude continuation fixture JSON");
    let body = fixture
        .get_mut("raw_body")
        .expect("fixture raw body")
        .as_object_mut()
        .expect("fixture raw body object");
    body.insert("model".to_string(), Value::String(model.to_string()));
    if let Some(result) = tool_result {
        let messages = body
            .get_mut("messages")
            .and_then(Value::as_array_mut)
            .expect("fixture continuation messages");
        let result_content = messages
            .last_mut()
            .and_then(|message| message.get_mut("content"))
            .and_then(Value::as_array_mut)
            .and_then(|parts| parts.first_mut())
            .and_then(|part| part.get_mut("content"))
            .expect("fixture immediate tool result");
        *result_content = Value::String(result.to_string());
    }
    NativeCase {
        name: "Claude Code",
        source: HarnessId::ClaudeCode,
        path: "/v1/messages",
        headers: &[("anthropic-beta", "claude-code-20250219")],
        body: Value::Object(body.clone()),
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

async fn generalization_server(upstream: &str) -> (TestServer, RealTraceCapture, TempDir) {
    let (cfg, config_path, config_dir) = template_config_with_mock(upstream);
    let assembled = bitrouter::build_app_with_path(&cfg, Some(&config_path))
        .await
        .expect("app assembles");
    for generated_identity in [
        ".installation.lock",
        "continuation.key",
        "correlation.key",
        "installation.id",
    ] {
        assert!(
            !template_root().join(generated_identity).exists(),
            "template tests must not generate {generated_identity} in the source tree"
        );
    }
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
    (TestServer::new(router), capture, config_dir)
}

fn template_config() -> config::Config {
    let path = template_root().join("bitrouter.yaml");
    let yaml = std::fs::read_to_string(&path).expect("read auto template");
    config::parse_with(&yaml, |_| None).expect("auto template config parses")
}

fn template_config_with_mock(upstream: &str) -> (config::Config, PathBuf, TempDir) {
    let mut config = template_config();
    config.server.skip_auth = true;
    config.database.url = "sqlite::memory:".to_string();

    // Production provider ids can install provider-specific authentication
    // (notably openai-codex OAuth). Keep the template assertions above tied to
    // the real ids, but give the HTTP test credential-independent mock ids.
    let mut strong_provider = config
        .providers
        .remove("openai-codex")
        .expect("template strong provider exists");
    let mut economy_provider = config
        .providers
        .remove("bitrouter")
        .expect("template economy provider exists");
    for provider in [&mut strong_provider, &mut economy_provider] {
        provider.api_base = upstream.to_string();
        provider.api_key = "test-key".to_string();
    }
    config
        .providers
        .insert("mock-strong".to_string(), strong_provider);
    config
        .providers
        .insert("mock-economy".to_string(), economy_provider);
    config.inherit_defaults = false;
    config
        .presets
        .get_mut("auto")
        .expect("template auto preset exists")
        .model = Some(MOCK_STRONG_MODEL.to_string());

    let config_dir = TempDir::new().expect("temporary mock policy directory");
    let config_path = config_dir.path().join("bitrouter.yaml");
    let lock_path = template_root().join("policy-lock.yaml");
    let lock_yaml = std::fs::read_to_string(&lock_path).expect("read auto template policy lock");
    let mut lock: PolicyLock =
        serde_saphyr::from_str(&lock_yaml).expect("parse auto template policy lock");
    let auto = lock
        .policies
        .get_mut("auto")
        .expect("template auto policy exists");
    auto.tiers
        .insert("strong".to_string(), MOCK_STRONG_MODEL.to_string());
    auto.tiers
        .insert("balanced".to_string(), MOCK_BALANCED_MODEL.to_string());
    auto.tiers
        .insert("economy".to_string(), MOCK_ECONOMY_MODEL.to_string());
    let mock_lock_path = config_dir.path().join("policy-lock.yaml");
    let mock_lock_yaml = serde_saphyr::to_string(&lock).expect("serialize mock policy lock");
    std::fs::write(&mock_lock_path, mock_lock_yaml).expect("write mock policy lock");
    config.policy.path = Some(mock_lock_path);

    (config, config_path, config_dir)
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
