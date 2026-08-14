use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axum_test::TestServer;
use bitrouter::policy_lock::PolicyLock;
use bitrouter::policy_table_router::{PolicyDecision, PolicyTableRouter};
use bitrouter::workflow_state::decision::{POLICY_DECISION_JSONL_ENV, PolicyDecisionRecord};
use bitrouter::workflow_state::fixture::WorkflowTraceFixture;
use bitrouter::workflow_state::ir::{AgentRole, HarnessId};
use bitrouter::workflow_state::predictive::{NextActionClass, NextStepRole};
use bitrouter::workflow_state::real_trace::{RealTraceCapture, TraceCaptureOptions};
use bitrouter::workflow_state::replay::ReplayEvaluator;
use bitrouter_sdk::config::{self, PolicyKeyStrategy, PolicyTableConfig, resolve_presets};
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

#[derive(Debug, PartialEq, Eq)]
struct RoutingDecisionView<'a> {
    request_key: &'a str,
    route_projection: &'a str,
    observed_route_projection: &'a str,
    predicted_role: Option<&'a str>,
    predicted_action: Option<&'a str>,
    prediction_reason_codes: &'a [String],
    selected_tier: Option<&'a str>,
}

fn routing_decision_view(decision: &PolicyDecision) -> RoutingDecisionView<'_> {
    RoutingDecisionView {
        request_key: &decision.request_key,
        route_projection: &decision.route_projection,
        observed_route_projection: &decision.observed_route_projection,
        predicted_role: decision.predicted_role.as_deref(),
        predicted_action: decision.predicted_action.as_deref(),
        prediction_reason_codes: &decision.prediction_reason_codes,
        selected_tier: decision.selected_tier.as_deref(),
    }
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
        assert_eq!(decision.request_key, "agent_route/v1|orchestrate|normal");
        assert_eq!(
            decision.selected_tier.as_deref(),
            Some("strong"),
            "broad repository openings stay on the orchestrator: {decision:?}"
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
async fn release_behavior_routes_three_stock_protocols_once_without_semantic_rewrites() {
    let _env_lock = DECISION_RECORDER_ENV_LOCK.lock().await;
    let upstream = mock_chat_upstream().await;
    let temp = TempDir::new().expect("temporary decision directory");
    let decisions_path = temp.path().join("decisions.jsonl");
    let _decision_env = DecisionRecorderEnv::set(&decisions_path);
    let (server, _, _config_dir) = generalization_server(&upstream.uri()).await;
    let instruction = "Read src/parser.rs and report the current error enum.";
    let schema = json!({
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"]
    });
    let cases = [
        (
            "/v1/chat/completions",
            json!({
                "model": "@auto",
                "messages": [{"role": "user", "content": instruction}],
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "description": "Read one file",
                        "parameters": schema
                    }
                }]
            }),
            "/choices/0/message/content",
        ),
        (
            "/v1/responses",
            json!({
                "model": "@auto",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": instruction
                }],
                "tools": [{
                    "type": "function",
                    "name": "read_file",
                    "description": "Read one file",
                    "parameters": schema
                }]
            }),
            "/output/0/content/0/text",
        ),
        (
            "/v1/messages",
            json!({
                "model": "@auto",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": instruction}],
                "tools": [{
                    "name": "read_file",
                    "description": "Read one file",
                    "input_schema": schema
                }]
            }),
            "/content/0/text",
        ),
    ];

    for (path, body, response_pointer) in cases {
        let response = server.post(path).json(&body).await;
        response.assert_status_ok();
        let response_body = response.json::<Value>();
        assert_eq!(
            response_body.pointer(response_pointer),
            Some(&Value::String("ok".into())),
            "{path} response semantics"
        );
    }

    let requests = upstream
        .received_requests()
        .await
        .expect("mock upstream request journal");
    assert_eq!(
        requests.len(),
        3,
        "one and only one upstream attempt per request"
    );
    for request in requests {
        let body: Value = serde_json::from_slice(&request.body).expect("forwarded JSON body");
        assert!(
            body["input"].to_string().contains(instruction),
            "canonical user input was rewritten: {body}"
        );
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["description"], "Read one file");
        assert_eq!(body["tools"][0]["parameters"], schema);
    }

    let decisions = PolicyDecisionRecord::load_jsonl(&decisions_path)
        .expect("release requests emit policy decisions");
    assert_eq!(decisions.len(), 3);
    for decision in decisions {
        assert_eq!(decision.request_key, "agent_route/v1|mechanical|normal");
        assert_eq!(decision.selected_tier.as_deref(), Some("economy"));
        assert_eq!(
            decision.predictor_contract_digest.as_deref(),
            Some("sha256:7483fb5fa02c0141f568b82287234895c666fef426789e32783bdd3a00cea3ec")
        );
        assert_eq!(
            decision.prediction_confidence_kind.as_deref(),
            Some("heuristic_margin")
        );
    }
}

#[tokio::test]
async fn predictive_routes_do_not_fallback_to_a_second_account_after_upstream_failure() {
    let _env_lock = DECISION_RECORDER_ENV_LOCK.lock().await;
    #[derive(Clone, Copy)]
    enum Failure {
        ServiceUnavailable,
        Timeout,
        InvalidResponse,
    }

    for failure in [
        Failure::ServiceUnavailable,
        Failure::Timeout,
        Failure::InvalidResponse,
    ] {
        let primary = MockServer::start().await;
        let template = match failure {
            Failure::ServiceUnavailable => ResponseTemplate::new(503),
            Failure::Timeout => ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(2))
                .set_body_json(json!({"status": "completed"})),
            Failure::InvalidResponse => {
                ResponseTemplate::new(200).set_body_raw("{", "application/json")
            }
        };
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(template)
            .mount(&primary)
            .await;
        let secondary = mock_chat_upstream().await;
        let (mut config, config_path, config_dir) = template_config_with_mock(&primary.uri());
        if matches!(failure, Failure::Timeout) {
            config.upstream.timeouts.total_secs = Some(1);
        }
        let provider = config
            .providers
            .get_mut("mock-economy")
            .expect("template economy provider exists");
        provider.account_strategy = config::AccountStrategy::Failover;
        provider.accounts = vec![
            config::ProviderAccount {
                api_key: "primary-key".into(),
                api_base: primary.uri(),
                label: "primary".into(),
            },
            config::ProviderAccount {
                api_key: "secondary-key".into(),
                api_base: secondary.uri(),
                label: "secondary".into(),
            },
        ];
        let (server, _, _config_dir) =
            generalization_server_from_config(config, config_path, config_dir).await;
        let instruction = "Read src/parser.rs and report the current error enum.";
        let schema = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        });
        let cases = [
            (
                "/v1/chat/completions",
                json!({
                    "model": "@auto",
                    "messages": [{"role": "user", "content": instruction}],
                    "tools": [{"type": "function", "function": {
                        "name": "read_file", "description": "Read one file",
                        "parameters": schema
                    }}]
                }),
            ),
            (
                "/v1/responses",
                json!({
                    "model": "@auto",
                    "input": [{"type": "message", "role": "user", "content": instruction}],
                    "tools": [{"type": "function", "name": "read_file",
                        "description": "Read one file", "parameters": schema}]
                }),
            ),
            (
                "/v1/messages",
                json!({
                    "model": "@auto",
                    "max_tokens": 64,
                    "messages": [{"role": "user", "content": instruction}],
                    "tools": [{"name": "read_file", "description": "Read one file",
                        "input_schema": schema}]
                }),
            ),
        ];

        for (path, body) in cases {
            let response = server.post(path).json(&body).await;
            assert!(
                response.status_code().is_server_error(),
                "{path} must surface the selected account failure instead of retrying: {}",
                response.status_code()
            );
        }

        let primary_requests = primary
            .received_requests()
            .await
            .expect("primary request journal");
        let secondary_requests = secondary
            .received_requests()
            .await
            .expect("secondary request journal");
        assert_eq!(
            primary_requests.len(),
            3,
            "one selected attempt per protocol"
        );
        assert_eq!(
            secondary_requests.len(),
            0,
            "predictive selection must not retry an unproven second account"
        );
        for request in primary_requests {
            let body: Value = serde_json::from_slice(&request.body).expect("forwarded JSON body");
            assert!(body["input"].to_string().contains(instruction));
            assert_eq!(body["tools"][0]["name"], "read_file");
            assert_eq!(body["tools"][0]["description"], "Read one file");
            assert_eq!(body["tools"][0]["parameters"], schema);
        }
    }
}

#[tokio::test]
async fn explicit_non_policy_routes_retain_generic_multi_account_fallback() {
    let _env_lock = DECISION_RECORDER_ENV_LOCK.lock().await;
    let primary = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&primary)
        .await;
    let secondary = mock_chat_upstream().await;
    let (mut config, config_path, config_dir) = template_config_with_mock(&primary.uri());
    let provider = config
        .providers
        .get_mut("mock-economy")
        .expect("template economy provider exists");
    provider.account_strategy = config::AccountStrategy::Failover;
    provider.accounts = vec![
        config::ProviderAccount {
            api_key: "primary-key".into(),
            api_base: primary.uri(),
            label: "primary".into(),
        },
        config::ProviderAccount {
            api_key: "secondary-key".into(),
            api_base: secondary.uri(),
            label: "secondary".into(),
        },
    ];
    let (server, _, _config_dir) =
        generalization_server_from_config(config, config_path, config_dir).await;

    server
        .post("/v1/chat/completions")
        .json(&json!({
            "model": MOCK_ECONOMY_MODEL,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .await
        .assert_status_ok();

    assert_eq!(
        primary
            .received_requests()
            .await
            .expect("primary request journal")
            .len(),
        1
    );
    assert_eq!(
        secondary
            .received_requests()
            .await
            .expect("secondary request journal")
            .len(),
        1,
        "non-policy routes retain the SDK's explicit generic fallback contract"
    );
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
        assert_eq!(decision.selected_tier.as_deref(), Some("balanced"));
        assert_eq!(
            decision.trajectory_completeness.as_deref(),
            Some("complete")
        );
    }
    for (decision, key) in decisions[1..10].iter().step_by(2).zip([
        "agent_route/v1|unknown|normal",
        "agent_route/v1|unknown|normal",
        "agent_route/v1|unknown|normal",
        "agent_route/v1|unknown|normal",
        "agent_route/v1|unknown|normal",
    ]) {
        assert_eq!(decision.request_key, key);
        assert_eq!(decision.selected_tier.as_deref(), Some("balanced"));
        assert_eq!(
            decision.trajectory_completeness.as_deref(),
            Some("complete")
        );
    }
    assert_eq!(decisions[11].request_key, "agent_route/v1|unknown|normal");
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
    assert_eq!(decisions[14].request_key, "agent_route/v1|unknown|normal");
    assert_eq!(decisions[14].static_tier.as_deref(), Some("balanced"));
    assert_eq!(decisions[14].selected_tier.as_deref(), Some("strong"));
    assert_eq!(
        decisions[14].trajectory_completeness.as_deref(),
        Some("incomplete")
    );
}

#[tokio::test]
async fn native_literal_histories_receive_exact_template_decisions() {
    let _env_lock = DECISION_RECORDER_ENV_LOCK.lock().await;
    let upstream = mock_chat_upstream().await;
    let temp = TempDir::new().expect("temporary decision directory");
    let decisions_path = temp.path().join("decisions.jsonl");
    let _decision_env = DecisionRecorderEnv::set(&decisions_path);
    let (server, capture, _config_dir) = generalization_server(&upstream.uri()).await;

    let mut scenarios = [
        (
            "edit",
            "agent_route/v1|unknown|normal",
            "agent_route/v1|verify|normal",
            terminus_action_case("@auto", "apply_patch <<'PATCH'\nPATCH"),
            claude_fixture_case("@auto:cost", ClaudeFixture::Edit, None),
            "balanced",
            MOCK_BALANCED_MODEL,
            "economy",
            MOCK_ECONOMY_MODEL,
        ),
        (
            "test",
            "agent_route/v1|unknown|normal",
            "agent_route/v1|finalize|normal",
            terminus_action_case("@auto", "cargo test -p bitrouter"),
            claude_fixture_case("@auto:cost", ClaudeFixture::Test, None),
            "balanced",
            MOCK_BALANCED_MODEL,
            "balanced",
            MOCK_BALANCED_MODEL,
        ),
        (
            "tool followup",
            "agent_route/v1|unknown|normal",
            "agent_route/v1|unknown|normal",
            terminus_tool_case("@auto", None),
            claude_fixture_case("@auto:cost", ClaudeFixture::ToolFollowup, None),
            "balanced",
            MOCK_BALANCED_MODEL,
            "balanced",
            MOCK_BALANCED_MODEL,
        ),
        (
            "recovery",
            "agent_route/v1|implement|guarded",
            "agent_route/v1|implement|guarded",
            terminus_tool_case("@auto", Some("error: cargo test failed")),
            claude_fixture_case(
                "@auto:cost",
                ClaudeFixture::ToolFollowup,
                Some("error: cargo test failed"),
            ),
            "strong",
            MOCK_STRONG_MODEL,
            "strong",
            MOCK_STRONG_MODEL,
        ),
    ];

    for (name, _, _, first, second, _, _, _, _) in &mut scenarios {
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
    for (
        (name, first_key, second_key, _, _, first_tier, first_model, second_tier, second_model),
        pair,
    ) in scenarios.iter().zip(decisions.chunks_exact(4))
    {
        for root in [&pair[0], &pair[2]] {
            assert_eq!(root.request_key, "agent_route/v1|unknown|normal");
            assert_eq!(root.selected_tier.as_deref(), Some("balanced"));
            assert_eq!(root.trajectory_completeness.as_deref(), Some("complete"));
        }
        assert_eq!(pair[1].request_key, *first_key, "{name} first source");
        assert_eq!(pair[3].request_key, *second_key, "{name} second source");
        assert_eq!(
            pair[1].selected_tier.as_deref(),
            Some(*first_tier),
            "{name} first source"
        );
        assert_eq!(
            pair[3].selected_tier.as_deref(),
            Some(*second_tier),
            "{name} second source"
        );
        assert_eq!(
            pair[1].selected_model.as_deref(),
            Some(*first_model),
            "{name} first source"
        );
        assert_eq!(
            pair[3].selected_model.as_deref(),
            Some(*second_model),
            "{name} second source"
        );
        assert_eq!(pair[1].trajectory_completeness.as_deref(), Some("complete"));
        assert_eq!(pair[3].trajectory_completeness.as_deref(), Some("complete"));
    }
}

#[test]
fn equivalent_native_histories_share_predictions_reasons_and_tiers() {
    let router = predictive_matrix_router();
    let cases = [
        (
            SemanticHistory::Opening,
            NextStepRole::Orchestrate,
            NextActionClass::ReasonOrPlan,
            "normal",
            "agent_route/v1|orchestrate|normal",
            &["opening_broad_goal"] as &[&str],
            "strong",
        ),
        (
            SemanticHistory::NarrowRead,
            NextStepRole::Mechanical,
            NextActionClass::InspectOrRead,
            "normal",
            "agent_route/v1|mechanical|normal",
            &["narrow_read_requested"],
            "economy",
        ),
        (
            SemanticHistory::PostRead,
            NextStepRole::Implement,
            NextActionClass::Mutate,
            "normal",
            "agent_route/v1|implement|normal",
            &["concrete_mutation_requested", "read_result_available"],
            "economy",
        ),
        (
            SemanticHistory::PostEdit,
            NextStepRole::Verify,
            NextActionClass::ExecuteOrTest,
            "normal",
            "agent_route/v1|verify|normal",
            &["concrete_mutation_requested", "mutation_result_available"],
            "economy",
        ),
        (
            SemanticHistory::FailedTest,
            NextStepRole::Implement,
            NextActionClass::Mutate,
            "guarded",
            "agent_route/v1|implement|guarded",
            &["concrete_mutation_requested", "test_failed_once"],
            "strong",
        ),
        (
            SemanticHistory::Finalization,
            NextStepRole::Finalize,
            NextActionClass::AnswerOrSummarize,
            "normal",
            "agent_route/v1|finalize|normal",
            &["concrete_mutation_requested", "progress_near_done"],
            "balanced",
        ),
    ];

    for (history, role, action, risk, key, reason_codes, tier) in cases {
        let fixtures = equivalent_history_fixtures(history, false);
        let summary = ReplayEvaluator.run(&fixtures);
        assert_eq!(summary.records.len(), 5, "{history:?}");
        for (fixture, record) in fixtures.iter().zip(&summary.records) {
            assert_eq!(
                record.predictive_projection.next_step_role, role,
                "{history:?} {}",
                fixture.id
            );
            assert_eq!(
                record.next_action_class, action,
                "{history:?} {}",
                fixture.id
            );
            assert_eq!(
                record.predictor_contract_digest,
                "sha256:7483fb5fa02c0141f568b82287234895c666fef426789e32783bdd3a00cea3ec",
                "{history:?} {}",
                fixture.id
            );
            assert_eq!(
                record.prediction_confidence_kind, "heuristic_margin",
                "{history:?} {}",
                fixture.id
            );
            assert_eq!(
                record.predictive_projection.risk.to_string(),
                risk,
                "{history:?} {}",
                fixture.id
            );
            assert_eq!(
                record.predictive_route_key.as_str(),
                key,
                "{history:?} {}",
                fixture.id
            );
            assert_eq!(
                record
                    .prediction_reason_codes
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                reason_codes,
                "{history:?} {}",
                fixture.id
            );
            assert_eq!(
                router
                    .decision_for(&fixture.prompt, &fixture.headers)
                    .selected_tier
                    .as_deref(),
                Some(tier),
                "{history:?} {}",
                fixture.id
            );
        }
    }
}

#[test]
fn private_headers_do_not_change_predictive_replay_or_selected_tier() {
    let router = predictive_matrix_router();
    for history in [
        SemanticHistory::Opening,
        SemanticHistory::NarrowRead,
        SemanticHistory::PostRead,
        SemanticHistory::PostEdit,
        SemanticHistory::FailedTest,
        SemanticHistory::Finalization,
    ] {
        let baseline = equivalent_history_fixtures(history, false);
        let decorated = equivalent_history_fixtures(history, true);
        let baseline_replay = ReplayEvaluator.run(&baseline);
        let decorated_replay = ReplayEvaluator.run(&decorated);

        for (((plain_fixture, decorated_fixture), plain), decorated) in baseline
            .iter()
            .zip(&decorated)
            .zip(&baseline_replay.records)
            .zip(&decorated_replay.records)
        {
            let plain_decision = router.decision_for(&plain_fixture.prompt, &plain_fixture.headers);
            let decorated_decision =
                router.decision_for(&decorated_fixture.prompt, &decorated_fixture.headers);
            assert_eq!(
                decorated.predictive_projection, plain.predictive_projection,
                "{history:?} {}",
                plain_fixture.id
            );
            assert_eq!(
                decorated.next_action_class, plain.next_action_class,
                "{history:?} {}",
                plain_fixture.id
            );
            assert_eq!(
                decorated.prediction_reason_codes, plain.prediction_reason_codes,
                "{history:?} {}",
                plain_fixture.id
            );
            assert_eq!(
                decorated_decision.request_key, plain_decision.request_key,
                "{history:?} {}",
                plain_fixture.id
            );
            assert_eq!(
                decorated_decision.route_projection, plain_decision.route_projection,
                "{history:?} {}",
                plain_fixture.id
            );
            assert_eq!(
                decorated_decision.observed_route_projection,
                plain_decision.observed_route_projection,
                "{history:?} {}",
                plain_fixture.id
            );
            assert_eq!(
                decorated_decision.predicted_role, plain_decision.predicted_role,
                "{history:?} {}",
                plain_fixture.id
            );
            assert_eq!(
                decorated_decision.predicted_action, plain_decision.predicted_action,
                "{history:?} {}",
                plain_fixture.id
            );
            assert_eq!(
                decorated_decision.prediction_reason_codes, plain_decision.prediction_reason_codes,
                "{history:?} {}",
                plain_fixture.id
            );
            assert_eq!(
                decorated_decision.selected_tier, plain_decision.selected_tier,
                "{history:?} {}",
                plain_fixture.id
            );
            assert_eq!(
                routing_decision_view(&decorated_decision),
                routing_decision_view(&plain_decision),
                "{history:?} {} full routing decision",
                plain_fixture.id
            );
            assert_eq!(
                plain_decision.request_key, plain.predictive_route_key,
                "{history:?} {}",
                plain_fixture.id
            );
            assert_ne!(plain_decision.workflow_identity.source, "explicit_headers");
            assert_eq!(
                decorated_decision.workflow_identity.role,
                AgentRole::Answers
            );
            assert_eq!(
                decorated_decision.workflow_identity.source,
                "explicit_headers"
            );
        }
    }
}

#[test]
fn full_decision_comparison_detects_projection_drift_with_the_same_tier() {
    let router = predictive_matrix_router();
    let post_read = equivalent_history_fixtures(SemanticHistory::PostRead, false)
        .into_iter()
        .next()
        .expect("post-read matrix has a generic fixture");
    let post_edit = equivalent_history_fixtures(SemanticHistory::PostEdit, false)
        .into_iter()
        .next()
        .expect("post-edit matrix has a generic fixture");

    let post_read_decision = router.decision_for(&post_read.prompt, &post_read.headers);
    let post_edit_decision = router.decision_for(&post_edit.prompt, &post_edit.headers);

    assert_eq!(post_read_decision.selected_tier.as_deref(), Some("economy"));
    assert_eq!(post_edit_decision.selected_tier.as_deref(), Some("economy"));
    assert_ne!(
        routing_decision_view(&post_read_decision),
        routing_decision_view(&post_edit_decision),
        "the header comparator must detect projection drift even when the tier collides"
    );
    assert_ne!(
        post_read_decision.request_key,
        post_edit_decision.request_key
    );
    assert_ne!(
        post_read_decision.observed_route_projection,
        post_edit_decision.observed_route_projection
    );
    assert_ne!(
        post_read_decision.predicted_role,
        post_edit_decision.predicted_role
    );
    assert_ne!(
        post_read_decision.predicted_action,
        post_edit_decision.predicted_action
    );
    assert_ne!(
        post_read_decision.prediction_reason_codes,
        post_edit_decision.prediction_reason_codes
    );
}

#[derive(Debug, Clone, Copy)]
enum SemanticHistory {
    Opening,
    NarrowRead,
    PostRead,
    PostEdit,
    FailedTest,
    Finalization,
}

#[derive(Clone, Copy)]
enum MatrixProtocol {
    Chat,
    Responses,
    Messages,
}

fn equivalent_history_fixtures(
    history: SemanticHistory,
    private_headers: bool,
) -> Vec<WorkflowTraceFixture> {
    [
        ("chat", MatrixProtocol::Chat, "generic"),
        ("responses", MatrixProtocol::Responses, "codex"),
        ("messages", MatrixProtocol::Messages, "claude_code"),
        ("hermes", MatrixProtocol::Chat, "hermes"),
        ("terminus", MatrixProtocol::Chat, "terminus_2"),
    ]
    .into_iter()
    .map(|(source, protocol, harness)| {
        let mut headers = match source {
            "responses" => json!({"user-agent": "codex-cli/1.0"}),
            "messages" => json!({"anthropic-beta": "claude-code-20250219"}),
            "hermes" => json!({"user-agent": "hermes-cli/1.0"}),
            _ => json!({}),
        };
        if private_headers {
            let object = headers
                .as_object_mut()
                .expect("matrix fixture headers are an object");
            object.insert("x-bitrouter-agent-role".into(), json!("answers"));
            object.insert("x-superpowers-phase".into(), json!("implementation"));
            object.insert("x-superpowers-skill".into(), json!("task-execution"));
            object.insert("x-superpowers-task".into(), json!("private-phase-task"));
            object.insert("x-superpowers-workflow".into(), json!("private-workflow"));
            object.insert("x-bitrouter-benchmark-id".into(), json!("private-case"));
            object.insert(
                "x-bitrouter-benchmark-run-id".into(),
                json!("private-bench"),
            );
            object.insert("x-bitrouter-task-id".into(), json!("private-task"));
        }
        let protocol_name = match protocol {
            MatrixProtocol::Chat => "chat_completions",
            MatrixProtocol::Responses => "responses",
            MatrixProtocol::Messages => "messages",
        };
        WorkflowTraceFixture::from_value(json!({
            "id": format!("{source}-{history:?}"),
            "harness": harness,
            "protocol": protocol_name,
            "headers": headers,
            "raw_body": semantic_history_body(history, protocol, source),
            "expected": {
                "state_kind": semantic_observed_state(history),
                "baseline_fingerprint": semantic_baseline(history, source),
                "confidence_min": 0.0
            }
        }))
        .expect("equivalent native history fixture parses")
    })
    .collect()
}

fn semantic_history_body(
    history: SemanticHistory,
    protocol: MatrixProtocol,
    source: &str,
) -> Value {
    let root = match history {
        SemanticHistory::Opening => "Investigate the repository architecture.",
        SemanticHistory::NarrowRead => "Read src/parser.rs and report the current error enum.",
        SemanticHistory::PostRead => "Implement the correction in src/parser.rs.",
        SemanticHistory::PostEdit | SemanticHistory::FailedTest | SemanticHistory::Finalization => {
            "Fix src/parser.rs."
        }
    };
    if matches!(protocol, MatrixProtocol::Chat) && source == "terminus" {
        return terminus_semantic_history_body(history, root);
    }
    let actions = match history {
        SemanticHistory::Opening | SemanticHistory::NarrowRead => Vec::new(),
        SemanticHistory::PostRead => vec![(
            "read-1",
            "Bash",
            r#"{"cmd":"cat src/parser.rs"}"#,
            "source contents",
        )],
        SemanticHistory::PostEdit => vec![(
            "edit-1",
            "Edit",
            r#"{"file_path":"src/parser.rs"}"#,
            "patch applied",
        )],
        SemanticHistory::FailedTest => vec![(
            "test-1",
            "Bash",
            r#"{"cmd":"cargo test -p parser"}"#,
            "error: parser test failed",
        )],
        SemanticHistory::Finalization => vec![
            (
                "edit-1",
                "Edit",
                r#"{"file_path":"src/parser.rs"}"#,
                "patch applied",
            ),
            (
                "test-1",
                "Bash",
                r#"{"cmd":"cargo test -p parser"}"#,
                "test result: ok. 12 passed; 0 failed",
            ),
        ],
    };

    match protocol {
        MatrixProtocol::Chat => {
            let mut messages = vec![json!({"role": "user", "content": root})];
            for (id, name, arguments, output) in actions {
                messages.push(json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments}
                    }]
                }));
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": output
                }));
            }
            let mut body = json!({"model": "inbound", "messages": messages});
            if source == "hermes" {
                body["metadata"] = json!({"job_id": "matrix-hermes-job"});
            }
            body
        }
        MatrixProtocol::Responses => {
            let mut input = vec![json!({"type": "message", "role": "user", "content": root})];
            for (id, name, arguments, output) in actions {
                input.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": arguments
                }));
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": id,
                    "output": output
                }));
            }
            json!({"model": "inbound", "input": input})
        }
        MatrixProtocol::Messages => {
            let mut messages = vec![json!({"role": "user", "content": root})];
            for (id, name, arguments, output) in actions {
                let input = serde_json::from_str::<Value>(arguments)
                    .expect("literal tool arguments are JSON");
                messages.push(json!({
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]
                }));
                messages.push(json!({
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": id, "content": output}]
                }));
            }
            json!({"model": "inbound", "max_tokens": 64, "messages": messages})
        }
    }
}

fn terminus_semantic_history_body(history: SemanticHistory, root: &str) -> Value {
    let mut messages = vec![
        json!({"role": "system", "content": terminus_contract()}),
        json!({"role": "user", "content": root}),
    ];
    let actions = match history {
        SemanticHistory::Opening | SemanticHistory::NarrowRead => Vec::new(),
        SemanticHistory::PostRead => vec![("cat src/parser.rs", "source contents")],
        SemanticHistory::PostEdit => vec![(
            "apply_patch <<'PATCH'\n*** Begin Patch\n*** End Patch\nPATCH",
            "patch applied",
        )],
        SemanticHistory::FailedTest => vec![("cargo test -p parser", "error: parser test failed")],
        SemanticHistory::Finalization => vec![
            (
                "apply_patch <<'PATCH'\n*** Begin Patch\n*** End Patch\nPATCH",
                "patch applied",
            ),
            (
                "cargo test -p parser",
                "test result: ok. 12 passed; 0 failed",
            ),
        ],
    };
    for (command, output) in actions {
        messages.push(json!({
            "role": "assistant",
            "content": json!({
                "analysis": "current state",
                "plan": "next command",
                "commands": [{"keystrokes": command, "duration": 0.1}],
                "task_complete": false
            }).to_string()
        }));
        messages.push(json!({"role": "user", "content": output}));
    }
    json!({"model": "inbound", "messages": messages})
}

fn semantic_observed_state(history: SemanticHistory) -> &'static str {
    match history {
        SemanticHistory::Opening | SemanticHistory::NarrowRead => "opening",
        SemanticHistory::PostRead => "tool_followup",
        SemanticHistory::PostEdit => "edit",
        SemanticHistory::FailedTest => "test",
        SemanticHistory::Finalization => "test",
    }
}

fn semantic_baseline(history: SemanticHistory, source: &str) -> &'static str {
    if source == "terminus"
        && !matches!(
            history,
            SemanticHistory::Opening | SemanticHistory::NarrowRead
        )
    {
        return "midstream";
    }
    match history {
        SemanticHistory::Opening | SemanticHistory::NarrowRead => "opening",
        SemanticHistory::PostRead | SemanticHistory::FailedTest | SemanticHistory::Finalization => {
            "after_Bash"
        }
        SemanticHistory::PostEdit => "after_Edit",
    }
}

fn predictive_matrix_router() -> PolicyTableRouter {
    PolicyTableRouter::from_config(&PolicyTableConfig {
        key_strategy: PolicyKeyStrategy::AgentTrace,
        tiers: HashMap::from([
            ("strong".to_string(), "vendor/strong".into()),
            ("economy".to_string(), "vendor/economy".into()),
            ("balanced".to_string(), "vendor/balanced".into()),
        ]),
        fingerprints: HashMap::from([
            (
                "agent_route/v1|orchestrate|normal".to_string(),
                "strong".to_string(),
            ),
            (
                "agent_route/v1|implement|normal".to_string(),
                "economy".to_string(),
            ),
            (
                "agent_route/v1|mechanical|normal".to_string(),
                "economy".to_string(),
            ),
            (
                "agent_route/v1|verify|normal".to_string(),
                "economy".to_string(),
            ),
            (
                "agent_route/v1|finalize|normal".to_string(),
                "balanced".to_string(),
            ),
        ]),
        default_tier: Some("strong".to_string()),
        tool_use_tier: Some("strong".to_string()),
        tool_safe_tiers: vec![
            "strong".to_string(),
            "economy".to_string(),
            "balanced".to_string(),
        ],
        adequacy: Default::default(),
    })
    .expect("predictive matrix policy has tiers")
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
    generalization_server_from_config(cfg, config_path, config_dir).await
}

async fn generalization_server_from_config(
    cfg: config::Config,
    config_path: PathBuf,
    config_dir: TempDir,
) -> (TestServer, RealTraceCapture, TempDir) {
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
        .insert("strong".to_string(), MOCK_STRONG_MODEL.into());
    auto.tiers
        .insert("balanced".to_string(), MOCK_BALANCED_MODEL.into());
    auto.tiers
        .insert("economy".to_string(), MOCK_ECONOMY_MODEL.into());
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
