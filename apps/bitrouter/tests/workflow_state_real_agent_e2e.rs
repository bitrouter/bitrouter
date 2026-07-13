#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use bitrouter::workflow_state::archive::TraceArchive;
use bitrouter::workflow_state::fixture::WorkflowTraceFixture;
use bitrouter::workflow_state::ir::{HarnessId, SessionConfidence, WorkflowStateKind};
use bitrouter::workflow_state::real_trace::{
    RealTraceCapture, TraceCaptureOptions, TraceSanitizer,
};
use bitrouter::workflow_state::replay::ReplayEvaluator;
use bitrouter_sdk::server::{AppState, RouterOptions, build_router_with_options};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const REAL_AGENT_SESSION_ID_A: &str = "00000000-0000-4000-8000-000000000001";
const REAL_AGENT_SESSION_ID_B: &str = "00000000-0000-4000-8000-000000000002";
const REAL_AGENT_SESSION_ID: &str = REAL_AGENT_SESSION_ID_A;

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires local Hermes CLI; run explicitly for real-agent workflow-state validation"]
async fn e2e_real_hermes_agent_traffic_is_captured_and_shadow_replayed() {
    let hermes = find_cli("hermes");

    let upstream = mock_chat_completions_upstream().await;
    let cfg = config_for_real_agent(&upstream.uri());
    let RealAgentServer {
        base_url,
        capture,
        server_task,
    } = serve_real_agent_router(&cfg, HarnessId::Hermes).await;
    let hermes_home = temp_hermes_home(&base_url, REAL_AGENT_SESSION_ID);

    let output = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new(hermes)
            .arg("--yolo")
            .arg("chat")
            .arg("--ignore-rules")
            .arg("--model")
            .arg("openai/bitrouter-hermes-tbench")
            .arg("-q")
            .arg("Reply with exactly: OK")
            .arg("-Q")
            .env("HERMES_HOME", hermes_home.path())
            .env("OPENAI_API_KEY", "bitrouter-local")
            .env("NO_PROXY", "127.0.0.1,localhost,::1,host.docker.internal")
            .env("no_proxy", "127.0.0.1,localhost,::1,host.docker.internal")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("hermes real-agent run timed out")
    .expect("spawn hermes");
    server_task.abort();

    assert!(
        output.status.success(),
        "Hermes failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_real_agent_replay(&capture, HarnessId::Hermes, "messages");
    assert_real_agent_session_confidence(
        &capture,
        HarnessId::Hermes,
        SessionConfidence::High,
        REAL_AGENT_SESSION_ID,
    );
}

fn tool_names(raw_body: &serde_json::Value) -> Vec<String> {
    raw_body
        .get("tools")
        .and_then(|tools| tools.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("function")
                        .and_then(|function| function.get("name"))
                        .or_else(|| tool.get("name"))
                        .and_then(|name| name.as_str())
                        .map(ToString::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires local Codex CLI; run explicitly for real-agent workflow-state validation"]
async fn e2e_real_codex_agent_traffic_is_captured_and_shadow_replayed() {
    let codex = find_cli("codex");

    let upstream = mock_responses_upstream().await;
    let cfg = config_for_real_agent(&upstream.uri());
    let RealAgentServer {
        base_url,
        capture,
        server_task,
    } = serve_real_agent_router(&cfg, HarnessId::Codex).await;
    let codex_home = temp_dir("bitrouter-codex-home");
    let codex_cwd = temp_dir("bitrouter-codex-cwd");
    let bitrouter_v1_base_url = format!("{}/v1", base_url.trim_end_matches('/'));

    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(codex)
            .arg("exec")
            .arg("--ignore-user-config")
            .arg("--ignore-rules")
            .arg("--ephemeral")
            .arg("--skip-git-repo-check")
            .arg("--json")
            .arg("-C")
            .arg(codex_cwd.path())
            .arg("-m")
            .arg("openai/bitrouter-codex-e2e")
            .arg("-c")
            .arg("model_provider=\"bitrouter_local\"")
            .arg("-c")
            .arg("model_providers.bitrouter_local.name=\"BitRouter Local\"")
            .arg("-c")
            .arg(format!(
                "model_providers.bitrouter_local.base_url=\"{bitrouter_v1_base_url}\""
            ))
            .arg("-c")
            .arg("model_providers.bitrouter_local.env_key=\"BITROUTER_LOCAL_API_KEY\"")
            .arg("-c")
            .arg("model_providers.bitrouter_local.wire_api=\"responses\"")
            .arg("-c")
            .arg("model_providers.bitrouter_local.requires_openai_auth=false")
            .arg("-c")
            .arg("model_providers.bitrouter_local.supports_websockets=false")
            .arg("-c")
            .arg(format!(
                "model_providers.bitrouter_local.http_headers={{\"x-bitrouter-workflow-session\"=\"{REAL_AGENT_SESSION_ID}\"}}"
            ))
            .arg("Reply with exactly: OK. Do not use tools.")
            .env("CODEX_HOME", codex_home.path())
            .env("BITROUTER_LOCAL_API_KEY", "bitrouter-local")
            .env("NO_PROXY", "127.0.0.1,localhost,::1,host.docker.internal")
            .env("no_proxy", "127.0.0.1,localhost,::1,host.docker.internal")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("codex real-agent run timed out")
    .expect("spawn codex");
    server_task.abort();

    assert!(
        output.status.success(),
        "Codex failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_real_agent_replay(&capture, HarnessId::Codex, "input");
    assert_real_agent_session_confidence(
        &capture,
        HarnessId::Codex,
        SessionConfidence::High,
        REAL_AGENT_SESSION_ID,
    );
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires local Claude CLI; run explicitly for real-agent workflow-state validation"]
async fn e2e_real_claude_agent_traffic_is_captured_and_shadow_replayed() {
    let claude = find_cli("claude");

    let upstream = mock_messages_upstream().await;
    let cfg = config_for_real_agent(&upstream.uri());
    let RealAgentServer {
        base_url,
        capture,
        server_task,
    } = serve_real_agent_router(&cfg, HarnessId::ClaudeCode).await;
    let claude_home = temp_dir("bitrouter-claude-home");

    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(claude)
            .arg("--bare")
            .arg("--print")
            .arg("--output-format")
            .arg("json")
            .arg("--model")
            .arg("anthropic/bitrouter-claude-e2e")
            .arg("--permission-mode")
            .arg("bypassPermissions")
            .arg("--tools")
            .arg("")
            .arg("--no-session-persistence")
            .arg("--session-id")
            .arg(REAL_AGENT_SESSION_ID)
            .arg("Reply with exactly: OK. Do not use tools.")
            .env("HOME", claude_home.path())
            .env("ANTHROPIC_BASE_URL", &base_url)
            .env("ANTHROPIC_API_KEY", "bitrouter-local")
            .env("ANTHROPIC_AUTH_TOKEN", "bitrouter-local")
            .env("NO_PROXY", "127.0.0.1,localhost,::1,host.docker.internal")
            .env("no_proxy", "127.0.0.1,localhost,::1,host.docker.internal")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("claude real-agent run timed out")
    .expect("spawn claude");
    server_task.abort();

    assert!(
        output.status.success(),
        "Claude failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_real_agent_replay(&capture, HarnessId::ClaudeCode, "messages");
    assert_real_agent_session_confidence(
        &capture,
        HarnessId::ClaudeCode,
        SessionConfidence::High,
        REAL_AGENT_SESSION_ID,
    );
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires local Codex CLI; run explicitly for multi-step real-agent workflow-state validation"]
async fn e2e_real_codex_multistep_tool_followup_is_captured_and_shadow_replayed() {
    let codex = find_cli("codex");

    let upstream = mock_responses_tool_roundtrip_upstream().await;
    let cfg = config_for_real_agent(&upstream.uri());
    let RealAgentServer {
        base_url,
        capture,
        server_task,
    } = serve_real_agent_router(&cfg, HarnessId::Codex).await;
    let codex_home = temp_dir("bitrouter-codex-multistep-home");
    let codex_cwd = temp_dir("bitrouter-codex-multistep-cwd");
    let bitrouter_v1_base_url = format!("{}/v1", base_url.trim_end_matches('/'));

    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(codex)
            .arg("exec")
            .arg("--ignore-user-config")
            .arg("--ignore-rules")
            .arg("--ephemeral")
            .arg("--skip-git-repo-check")
            .arg("--json")
            .arg("--dangerously-bypass-approvals-and-sandbox")
            .arg("-C")
            .arg(codex_cwd.path())
            .arg("-m")
            .arg("openai/bitrouter-codex-e2e")
            .arg("-c")
            .arg("model_provider=\"bitrouter_local\"")
            .arg("-c")
            .arg("model_providers.bitrouter_local.name=\"BitRouter Local\"")
            .arg("-c")
            .arg(format!(
                "model_providers.bitrouter_local.base_url=\"{bitrouter_v1_base_url}\""
            ))
            .arg("-c")
            .arg("model_providers.bitrouter_local.env_key=\"BITROUTER_LOCAL_API_KEY\"")
            .arg("-c")
            .arg("model_providers.bitrouter_local.wire_api=\"responses\"")
            .arg("-c")
            .arg("model_providers.bitrouter_local.requires_openai_auth=false")
            .arg("-c")
            .arg("model_providers.bitrouter_local.supports_websockets=false")
            .arg("-c")
            .arg(format!(
                "model_providers.bitrouter_local.http_headers={{\"x-bitrouter-workflow-session\"=\"{REAL_AGENT_SESSION_ID}\"}}"
            ))
            .arg("Run a harmless shell command to print codex-tool-ok, then answer OK.")
            .env("CODEX_HOME", codex_home.path())
            .env("BITROUTER_LOCAL_API_KEY", "bitrouter-local")
            .env("NO_PROXY", "127.0.0.1,localhost,::1,host.docker.internal")
            .env("no_proxy", "127.0.0.1,localhost,::1,host.docker.internal")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("codex multi-step real-agent run timed out")
    .expect("spawn codex");
    server_task.abort();

    assert!(
        output.status.success(),
        "Codex multi-step failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_multistep_real_agent_replay(&capture, HarnessId::Codex, "input");
    assert_real_agent_session_confidence(
        &capture,
        HarnessId::Codex,
        SessionConfidence::High,
        REAL_AGENT_SESSION_ID,
    );
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires local Hermes CLI; run explicitly for multi-step real-agent workflow-state validation"]
async fn e2e_real_hermes_multistep_tool_followup_is_captured_and_shadow_replayed() {
    let hermes = find_cli("hermes");

    let upstream = mock_chat_completions_tool_roundtrip_upstream().await;
    let cfg = config_for_real_agent(&upstream.uri());
    let RealAgentServer {
        base_url,
        capture,
        server_task,
    } = serve_real_agent_router(&cfg, HarnessId::Hermes).await;
    let hermes_home = temp_hermes_home(&base_url, REAL_AGENT_SESSION_ID);

    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(hermes)
            .arg("--yolo")
            .arg("chat")
            .arg("--ignore-rules")
            .arg("--model")
            .arg("openai/bitrouter-hermes-tbench")
            .arg("-q")
            .arg("Run a harmless shell command to print hermes-tool-ok, then answer OK.")
            .arg("-Q")
            .env("HERMES_HOME", hermes_home.path())
            .env("OPENAI_API_KEY", "bitrouter-local")
            .env("NO_PROXY", "127.0.0.1,localhost,::1,host.docker.internal")
            .env("no_proxy", "127.0.0.1,localhost,::1,host.docker.internal")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("hermes multi-step real-agent run timed out")
    .expect("spawn hermes");
    server_task.abort();

    assert!(
        output.status.success(),
        "Hermes multi-step failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_multistep_real_agent_replay(&capture, HarnessId::Hermes, "messages");
    assert_real_agent_session_confidence(
        &capture,
        HarnessId::Hermes,
        SessionConfidence::High,
        REAL_AGENT_SESSION_ID,
    );
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires local Claude CLI; run explicitly for multi-step real-agent workflow-state validation"]
async fn e2e_real_claude_multistep_tool_followup_is_captured_and_shadow_replayed() {
    let claude = find_cli("claude");

    let upstream = mock_messages_tool_roundtrip_upstream().await;
    let cfg = config_for_real_agent(&upstream.uri());
    let RealAgentServer {
        base_url,
        capture,
        server_task,
    } = serve_real_agent_router(&cfg, HarnessId::ClaudeCode).await;
    let claude_home = temp_dir("bitrouter-claude-multistep-home");

    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(claude)
            .arg("--bare")
            .arg("--print")
            .arg("--output-format")
            .arg("json")
            .arg("--model")
            .arg("anthropic/bitrouter-claude-e2e")
            .arg("--permission-mode")
            .arg("bypassPermissions")
            .arg("--tools")
            .arg("Bash")
            .arg("--no-session-persistence")
            .arg("--session-id")
            .arg(REAL_AGENT_SESSION_ID)
            .arg("Run a harmless shell command to print claude-tool-ok, then answer OK.")
            .env("HOME", claude_home.path())
            .env("ANTHROPIC_BASE_URL", &base_url)
            .env("ANTHROPIC_API_KEY", "bitrouter-local")
            .env("ANTHROPIC_AUTH_TOKEN", "bitrouter-local")
            .env("NO_PROXY", "127.0.0.1,localhost,::1,host.docker.internal")
            .env("no_proxy", "127.0.0.1,localhost,::1,host.docker.internal")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("claude multi-step real-agent run timed out")
    .expect("spawn claude");
    server_task.abort();

    assert!(
        output.status.success(),
        "Claude multi-step failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_multistep_real_agent_replay(&capture, HarnessId::ClaudeCode, "messages");
    assert_real_agent_session_confidence(
        &capture,
        HarnessId::ClaudeCode,
        SessionConfidence::High,
        REAL_AGENT_SESSION_ID,
    );
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires local Hermes CLI; run explicitly for multi-session real-agent workflow-state validation"]
async fn e2e_real_hermes_two_sessions_have_distinct_high_confidence_keys() {
    let hermes = find_cli("hermes");

    let upstream = mock_chat_completions_upstream().await;
    let cfg = config_for_real_agent(&upstream.uri());
    let RealAgentServer {
        base_url,
        capture,
        server_task,
    } = serve_real_agent_router(&cfg, HarnessId::Hermes).await;

    run_hermes_single_turn(&hermes, &base_url, REAL_AGENT_SESSION_ID_A).await;
    run_hermes_single_turn(&hermes, &base_url, REAL_AGENT_SESSION_ID_B).await;
    server_task.abort();

    assert_real_agent_replay(&capture, HarnessId::Hermes, "messages");
    assert_real_agent_session_set(
        &capture,
        HarnessId::Hermes,
        SessionConfidence::High,
        &[REAL_AGENT_SESSION_ID_A, REAL_AGENT_SESSION_ID_B],
    );
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires local Codex CLI; run explicitly for multi-session real-agent workflow-state validation"]
async fn e2e_real_codex_two_sessions_have_distinct_high_confidence_keys() {
    let codex = find_cli("codex");

    let upstream = mock_responses_upstream().await;
    let cfg = config_for_real_agent(&upstream.uri());
    let RealAgentServer {
        base_url,
        capture,
        server_task,
    } = serve_real_agent_router(&cfg, HarnessId::Codex).await;

    run_codex_single_turn(&codex, &base_url, REAL_AGENT_SESSION_ID_A).await;
    run_codex_single_turn(&codex, &base_url, REAL_AGENT_SESSION_ID_B).await;
    server_task.abort();

    assert_real_agent_replay(&capture, HarnessId::Codex, "input");
    assert_real_agent_session_set(
        &capture,
        HarnessId::Codex,
        SessionConfidence::High,
        &[REAL_AGENT_SESSION_ID_A, REAL_AGENT_SESSION_ID_B],
    );
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires local Claude CLI; run explicitly for multi-session real-agent workflow-state validation"]
async fn e2e_real_claude_two_sessions_have_distinct_high_confidence_keys() {
    let claude = find_cli("claude");

    let upstream = mock_messages_upstream().await;
    let cfg = config_for_real_agent(&upstream.uri());
    let RealAgentServer {
        base_url,
        capture,
        server_task,
    } = serve_real_agent_router(&cfg, HarnessId::ClaudeCode).await;

    run_claude_single_turn(&claude, &base_url, REAL_AGENT_SESSION_ID_A).await;
    run_claude_single_turn(&claude, &base_url, REAL_AGENT_SESSION_ID_B).await;
    server_task.abort();

    assert_real_agent_replay(&capture, HarnessId::ClaudeCode, "messages");
    assert_real_agent_session_set(
        &capture,
        HarnessId::ClaudeCode,
        SessionConfidence::High,
        &[REAL_AGENT_SESSION_ID_A, REAL_AGENT_SESSION_ID_B],
    );
}

fn find_cli(name: &str) -> String {
    match std::process::Command::new("sh")
        .arg("-lc")
        .arg(format!("command -v {name}"))
        .output()
    {
        Ok(output) if output.status.success() => String::from_utf8(output.stdout)
            .expect("cli path is utf8")
            .trim()
            .to_string(),
        _ => panic!("{name} CLI is required for this real-agent E2E test"),
    }
}

async fn run_hermes_single_turn(hermes: &str, base_url: &str, session_id: &str) {
    let hermes_home = temp_hermes_home(base_url, session_id);

    let output = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new(hermes)
            .arg("--yolo")
            .arg("chat")
            .arg("--ignore-rules")
            .arg("--model")
            .arg("openai/bitrouter-hermes-tbench")
            .arg("-q")
            .arg("Reply with exactly: OK")
            .arg("-Q")
            .env("HERMES_HOME", hermes_home.path())
            .env("OPENAI_API_KEY", "bitrouter-local")
            .env("NO_PROXY", "127.0.0.1,localhost,::1,host.docker.internal")
            .env("no_proxy", "127.0.0.1,localhost,::1,host.docker.internal")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("hermes real-agent run timed out")
    .expect("spawn hermes");

    assert_agent_output_success("Hermes", output);
}

async fn run_codex_single_turn(codex: &str, base_url: &str, session_id: &str) {
    let codex_home = temp_dir("bitrouter-codex-two-session-home");
    let codex_cwd = temp_dir("bitrouter-codex-two-session-cwd");
    let bitrouter_v1_base_url = format!("{}/v1", base_url.trim_end_matches('/'));

    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(codex)
            .arg("exec")
            .arg("--ignore-user-config")
            .arg("--ignore-rules")
            .arg("--ephemeral")
            .arg("--skip-git-repo-check")
            .arg("--json")
            .arg("-C")
            .arg(codex_cwd.path())
            .arg("-m")
            .arg("openai/bitrouter-codex-e2e")
            .arg("-c")
            .arg("model_provider=\"bitrouter_local\"")
            .arg("-c")
            .arg("model_providers.bitrouter_local.name=\"BitRouter Local\"")
            .arg("-c")
            .arg(format!(
                "model_providers.bitrouter_local.base_url=\"{bitrouter_v1_base_url}\""
            ))
            .arg("-c")
            .arg("model_providers.bitrouter_local.env_key=\"BITROUTER_LOCAL_API_KEY\"")
            .arg("-c")
            .arg("model_providers.bitrouter_local.wire_api=\"responses\"")
            .arg("-c")
            .arg("model_providers.bitrouter_local.requires_openai_auth=false")
            .arg("-c")
            .arg("model_providers.bitrouter_local.supports_websockets=false")
            .arg("-c")
            .arg(format!(
                "model_providers.bitrouter_local.http_headers={{\"x-bitrouter-workflow-session\"=\"{session_id}\"}}"
            ))
            .arg("Reply with exactly: OK. Do not use tools.")
            .env("CODEX_HOME", codex_home.path())
            .env("BITROUTER_LOCAL_API_KEY", "bitrouter-local")
            .env("NO_PROXY", "127.0.0.1,localhost,::1,host.docker.internal")
            .env("no_proxy", "127.0.0.1,localhost,::1,host.docker.internal")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("codex real-agent run timed out")
    .expect("spawn codex");

    assert_agent_output_success("Codex", output);
}

async fn run_claude_single_turn(claude: &str, base_url: &str, session_id: &str) {
    let claude_home = temp_dir("bitrouter-claude-two-session-home");

    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(claude)
            .arg("--bare")
            .arg("--print")
            .arg("--output-format")
            .arg("json")
            .arg("--model")
            .arg("anthropic/bitrouter-claude-e2e")
            .arg("--permission-mode")
            .arg("bypassPermissions")
            .arg("--tools")
            .arg("")
            .arg("--no-session-persistence")
            .arg("--session-id")
            .arg(session_id)
            .arg("Reply with exactly: OK. Do not use tools.")
            .env("HOME", claude_home.path())
            .env("ANTHROPIC_BASE_URL", base_url)
            .env("ANTHROPIC_API_KEY", "bitrouter-local")
            .env("ANTHROPIC_AUTH_TOKEN", "bitrouter-local")
            .env("NO_PROXY", "127.0.0.1,localhost,::1,host.docker.internal")
            .env("no_proxy", "127.0.0.1,localhost,::1,host.docker.internal")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("claude real-agent run timed out")
    .expect("spawn claude");

    assert_agent_output_success("Claude", output);
}

fn assert_agent_output_success(agent: &str, output: std::process::Output) {
    assert!(
        output.status.success(),
        "{agent} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn serve_real_agent_router(
    cfg: &bitrouter_sdk::config::Config,
    harness: HarnessId,
) -> RealAgentServer {
    let assembled = bitrouter::build_app(cfg).await.expect("app assembles");
    let capture = RealTraceCapture::new(TraceCaptureOptions {
        harness,
        session_header: Some("x-bitrouter-workflow-session".to_string()),
        archive_path: None,
    });

    let state = AppState {
        language_model: assembled.app.language_model().unwrap().clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    };
    let router = build_router_with_options(
        state,
        RouterOptions::default().with_router_wrapper(capture.router_wrapper()),
    );
    let (base_url, server_task) = serve_router(router).await;
    RealAgentServer {
        base_url,
        capture,
        server_task,
    }
}

struct RealAgentServer {
    base_url: String,
    capture: RealTraceCapture,
    server_task: tokio::task::JoinHandle<()>,
}

fn assert_real_agent_replay(
    capture: &RealTraceCapture,
    harness: HarnessId,
    required_raw_body_field: &str,
) {
    let records = capture.records();
    eprintln!(
        "captured_real_agent_traces={}; paths={:?}; tool_names={:?}",
        records.len(),
        records
            .iter()
            .map(|record| (
                &record.path,
                record.outcome.http_status,
                &record.outcome.status
            ))
            .collect::<Vec<_>>(),
        records
            .iter()
            .map(|record| tool_names(&record.raw_body))
            .collect::<Vec<_>>()
    );

    let fixtures = capture.replay_fixtures().expect("captured fixtures replay");
    assert!(
        !fixtures.is_empty(),
        "real {harness:?} traffic must produce at least one captured fixture"
    );
    assert!(
        fixtures
            .iter()
            .all(|f| { f.harness == harness && f.raw_body.get(required_raw_body_field).is_some() }),
        "captured fixtures must be real {harness:?} requests: {}",
        fixture_summary(&fixtures)
    );

    let summary = ReplayEvaluator.run(&fixtures);
    eprintln!("shadow_replay_summary={summary:#?}");
    assert_eq!(summary.total, fixtures.len(), "{summary:#?}");
    assert_eq!(summary.covered, fixtures.len(), "{summary:#?}");
    assert!(summary.ir_bucket_count >= 1, "{summary:#?}");
    assert_real_agent_archive_roundtrip(capture, harness, required_raw_body_field);
}

fn assert_real_agent_archive_roundtrip(
    capture: &RealTraceCapture,
    harness: HarnessId,
    required_raw_body_field: &str,
) {
    let temp = temp_dir("bitrouter-real-agent-trace-archive");
    let path = temp.path().join("traces.jsonl");
    let records = capture.records();
    TraceArchive::write_jsonl(&path, &records, &TraceSanitizer::default())
        .expect("real agent traces export to jsonl");
    let fixtures = TraceArchive::read_replay_fixtures(&path)
        .expect("real agent trace archive imports as replay fixtures");
    assert_eq!(fixtures.len(), records.len());
    assert!(
        fixtures
            .iter()
            .all(|f| { f.harness == harness && f.raw_body.get(required_raw_body_field).is_some() }),
        "archived fixtures must be real {harness:?} requests: {}",
        fixture_summary(&fixtures)
    );
}

fn assert_multistep_real_agent_replay(
    capture: &RealTraceCapture,
    harness: HarnessId,
    required_raw_body_field: &str,
) {
    assert_real_agent_replay(capture, harness.clone(), required_raw_body_field);
    let fixtures = capture.replay_fixtures().expect("captured fixtures replay");
    assert!(
        fixtures.len() >= 2,
        "multi-step real {harness:?} traffic must produce at least two captured fixtures: {}",
        fixture_summary(&fixtures)
    );
    assert!(
        fixtures
            .iter()
            .any(|fixture| fixture.expected.state_kind == WorkflowStateKind::ToolFollowup),
        "multi-step real {harness:?} traffic must include a tool-followup workflow state: {}",
        fixture_summary(&fixtures)
    );
}

fn assert_real_agent_session_confidence(
    capture: &RealTraceCapture,
    harness: HarnessId,
    expected: SessionConfidence,
    expected_key: &str,
) {
    let fixtures = capture.replay_fixtures().expect("captured fixtures replay");
    assert!(
        !fixtures.is_empty(),
        "real {harness:?} traffic must produce session-bearing fixtures"
    );
    let mismatches = fixtures
        .iter()
        .filter(|fixture| fixture.harness == harness)
        .filter(|fixture| {
            let session = bitrouter::workflow_state::replay::extract_fixture_ir(fixture).session;
            session.confidence != expected || session.key.as_deref() != Some(expected_key)
        })
        .map(|fixture| {
            let ir = bitrouter::workflow_state::replay::extract_fixture_ir(fixture);
            format!(
                "{}:{:?}:{:?}:{:?}:{:?}",
                fixture.id,
                fixture.harness,
                ir.session.confidence,
                ir.session.key,
                ir.session.source
            )
        })
        .collect::<Vec<_>>();
    assert!(
        mismatches.is_empty(),
        "real {harness:?} traffic must have {expected:?} session confidence: {mismatches:?}"
    );
}

fn assert_real_agent_session_set(
    capture: &RealTraceCapture,
    harness: HarnessId,
    expected: SessionConfidence,
    expected_keys: &[&str],
) -> BTreeMap<String, usize> {
    let fixtures = capture.replay_fixtures().expect("captured fixtures replay");
    assert!(
        !fixtures.is_empty(),
        "real {harness:?} traffic must produce session-bearing fixtures"
    );

    let mut counts = BTreeMap::new();
    let mut mismatches = Vec::new();
    for fixture in fixtures.iter().filter(|fixture| fixture.harness == harness) {
        let ir = bitrouter::workflow_state::replay::extract_fixture_ir(fixture);
        if ir.session.confidence != expected || ir.session.key.is_none() {
            mismatches.push(format!(
                "{}:{:?}:{:?}:{:?}:{:?}",
                fixture.id,
                fixture.harness,
                ir.session.confidence,
                ir.session.key,
                ir.session.source
            ));
            continue;
        }
        let key = ir.session.key.expect("checked session key exists");
        *counts.entry(key).or_insert(0) += 1;
    }

    assert!(
        mismatches.is_empty(),
        "real {harness:?} traffic must have {expected:?} session confidence: {mismatches:?}"
    );
    let observed = counts.keys().cloned().collect::<BTreeSet<_>>();
    let expected = expected_keys
        .iter()
        .map(|key| (*key).to_string())
        .collect::<BTreeSet<_>>();
    eprintln!("session_counts={counts:?}");
    assert_eq!(
        observed,
        expected,
        "real {harness:?} traffic must preserve distinct session keys: {}",
        fixture_summary(&fixtures)
    );
    counts
}

fn fixture_summary(fixtures: &[WorkflowTraceFixture]) -> String {
    let summary = fixtures
        .iter()
        .map(|fixture| {
            format!(
                "{}:{:?}/{:?}/{:?}/tools={:?}",
                fixture.id,
                fixture.harness,
                fixture.protocol,
                fixture.expected.state_kind,
                tool_names(&fixture.raw_body)
            )
        })
        .collect::<Vec<_>>();
    format!("[{}]", summary.join(", "))
}

async fn mock_chat_completions_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    r#"data: {"id":"chatcmpl-real-agent-e2e","object":"chat.completion.chunk","model":"test-model","choices":[{"index":0,"delta":{"role":"assistant","content":"OK"},"finish_reason":null}]}

data: {"id":"chatcmpl-real-agent-e2e","object":"chat.completion.chunk","model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":1,"total_tokens":21}}

data: [DONE]

"#,
                ),
        )
        .mount(&server)
        .await;
    server
}

async fn mock_chat_completions_tool_roundtrip_upstream() -> MockServer {
    let server = MockServer::start().await;
    let call_count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |_req: &Request| {
            let n = call_count.fetch_add(1, Ordering::SeqCst);
            let body = if n == 0 {
                chat_completions_tool_call_sse_body()
            } else {
                chat_completions_text_sse_body("OK")
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body)
        })
        .mount(&server)
        .await;
    server
}

async fn mock_responses_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses_sse_body()),
        )
        .mount(&server)
        .await;
    server
}

fn chat_completions_text_sse_body(text: &str) -> String {
    format!(
        r#"data: {{"id":"chatcmpl-real-agent-e2e","object":"chat.completion.chunk","model":"test-model","choices":[{{"index":0,"delta":{{"role":"assistant","content":{text_json}}},"finish_reason":null}}]}}

data: {{"id":"chatcmpl-real-agent-e2e","object":"chat.completion.chunk","model":"test-model","choices":[{{"index":0,"delta":{{}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":20,"completion_tokens":1,"total_tokens":21}}}}

data: [DONE]

"#,
        text_json = serde_json::to_string(text).expect("text serializes")
    )
}

fn chat_completions_tool_call_sse_body() -> String {
    r#"data: {"id":"chatcmpl-real-agent-tool","object":"chat.completion.chunk","model":"test-model","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_real_agent_e2e","type":"function","function":{"name":"terminal","arguments":""}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-real-agent-tool","object":"chat.completion.chunk","model":"test-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":\"printf hermes-tool-ok\",\"timeout\":5}"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl-real-agent-tool","object":"chat.completion.chunk","model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":20,"completion_tokens":1,"total_tokens":21}}

data: [DONE]

"#
    .to_string()
}

async fn mock_messages_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(messages_sse_body()),
        )
        .mount(&server)
        .await;
    server
}

async fn mock_responses_tool_roundtrip_upstream() -> MockServer {
    let server = MockServer::start().await;
    let call_count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(move |_req: &Request| {
            let n = call_count.fetch_add(1, Ordering::SeqCst);
            let body = if n == 0 {
                responses_tool_call_sse_body()
            } else {
                responses_sse_body()
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body)
        })
        .mount(&server)
        .await;
    server
}

async fn mock_messages_tool_roundtrip_upstream() -> MockServer {
    let server = MockServer::start().await;
    let call_count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(move |req: &Request| {
            let body_json = serde_json::from_slice::<serde_json::Value>(&req.body).ok();
            let has_tools = body_json
                .as_ref()
                .and_then(|body| body.get("tools"))
                .and_then(|tools| tools.as_array())
                .is_some_and(|tools| !tools.is_empty());
            let body = if has_tools {
                let n = call_count.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    messages_tool_call_sse_body()
                } else {
                    messages_sse_body()
                }
            } else {
                messages_text_sse_body(r#"{"title":"Real agent e2e"}"#)
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body)
        })
        .mount(&server)
        .await;
    server
}

fn responses_sse_body() -> String {
    let item = json!({
        "id": "msg-real-agent-e2e",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": "OK", "annotations": [] }]
    });
    let completed = json!({
        "id": "resp-real-agent-e2e",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "test-responses",
        "output": [item],
        "usage": {
            "input_tokens": 20,
            "output_tokens": 1,
            "total_tokens": 21
        }
    });
    let events = [
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": "resp-real-agent-e2e",
                "object": "response",
                "created_at": 0,
                "status": "in_progress",
                "model": "test-responses",
                "output": []
            }
        }),
        json!({
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": {
                "id": "msg-real-agent-e2e",
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": []
            }
        }),
        json!({
            "type": "response.content_part.added",
            "sequence_number": 2,
            "item_id": "msg-real-agent-e2e",
            "output_index": 0,
            "content_index": 0,
            "part": { "type": "output_text", "text": "", "annotations": [] }
        }),
        json!({
            "type": "response.output_text.delta",
            "sequence_number": 3,
            "item_id": "msg-real-agent-e2e",
            "output_index": 0,
            "content_index": 0,
            "delta": "OK"
        }),
        json!({
            "type": "response.output_text.done",
            "sequence_number": 4,
            "item_id": "msg-real-agent-e2e",
            "output_index": 0,
            "content_index": 0,
            "text": "OK"
        }),
        json!({
            "type": "response.content_part.done",
            "sequence_number": 5,
            "item_id": "msg-real-agent-e2e",
            "output_index": 0,
            "content_index": 0,
            "part": { "type": "output_text", "text": "OK", "annotations": [] }
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
            "response": completed
        }),
    ];
    events
        .iter()
        .map(|event| {
            let event_type = event["type"].as_str().expect("event type");
            format!("event: {event_type}\ndata: {event}\n\n")
        })
        .collect()
}

fn responses_tool_call_sse_body() -> String {
    let item = json!({
        "id": "fc-real-agent-e2e",
        "type": "function_call",
        "status": "completed",
        "call_id": "call_real_agent_e2e",
        "name": "exec_command",
        "arguments": "{\"cmd\":\"printf codex-tool-ok\",\"yield_time_ms\":1000,\"max_output_tokens\":2000}"
    });
    let completed = json!({
        "id": "resp-real-agent-tool",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "test-responses",
        "output": [item],
        "usage": {
            "input_tokens": 20,
            "output_tokens": 1,
            "total_tokens": 21
        }
    });
    let events = [
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": "resp-real-agent-tool",
                "object": "response",
                "created_at": 0,
                "status": "in_progress",
                "model": "test-responses",
                "output": []
            }
        }),
        json!({
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": {
                "id": "fc-real-agent-e2e",
                "type": "function_call",
                "status": "in_progress",
                "call_id": "call_real_agent_e2e",
                "name": "exec_command",
                "arguments": ""
            }
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "sequence_number": 2,
            "item_id": "fc-real-agent-e2e",
            "output_index": 0,
            "delta": "{\"cmd\":\"printf codex-tool-ok\",\"yield_time_ms\":1000,\"max_output_tokens\":2000}"
        }),
        json!({
            "type": "response.function_call_arguments.done",
            "sequence_number": 3,
            "item_id": "fc-real-agent-e2e",
            "output_index": 0,
            "arguments": "{\"cmd\":\"printf codex-tool-ok\",\"yield_time_ms\":1000,\"max_output_tokens\":2000}"
        }),
        json!({
            "type": "response.output_item.done",
            "sequence_number": 4,
            "output_index": 0,
            "item": item
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 5,
            "response": completed
        }),
    ];
    events
        .iter()
        .map(|event| {
            let event_type = event["type"].as_str().expect("event type");
            format!("event: {event_type}\ndata: {event}\n\n")
        })
        .collect()
}

fn messages_sse_body() -> String {
    messages_text_sse_body("OK")
}

fn messages_text_sse_body(text: &str) -> String {
    let events = [
        (
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg-real-agent-e2e",
                    "type": "message",
                    "role": "assistant",
                    "model": "test-messages",
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": 20, "output_tokens": 0 }
                }
            }),
        ),
        (
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" }
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": text }
            }),
        ),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                "usage": { "output_tokens": 1 }
            }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ];
    events
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect()
}

fn messages_tool_call_sse_body() -> String {
    let events = [
        (
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg-real-agent-tool",
                    "type": "message",
                    "role": "assistant",
                    "model": "test-messages",
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": 20, "output_tokens": 0 }
                }
            }),
        ),
        (
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_real_agent_e2e",
                    "name": "Bash",
                    "input": {
                        "command": "printf claude-tool-ok",
                        "description": "print a fixed marker"
                    }
                }
            }),
        ),
        (
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "tool_use", "stop_sequence": null },
                "usage": { "output_tokens": 1 }
            }),
        ),
        ("message_stop", json!({ "type": "message_stop" })),
    ];
    events
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect()
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn temp_dir(prefix: &str) -> TempDir {
    let path = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time after unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).expect("create temporary HERMES_HOME");
    TempDir { path }
}

fn temp_hermes_home(bitrouter_base_url: &str, session_id: &str) -> TempDir {
    let path = temp_dir("bitrouter-hermes-real-agent-e2e");
    let bitrouter_v1_base_url = format!("{}/v1", bitrouter_base_url.trim_end_matches('/'));
    std::fs::write(
        path.path().join("config.yaml"),
        format!(
            r#"
model:
  provider: custom
  default: openai/bitrouter-hermes-tbench
  base_url: "{bitrouter_v1_base_url}"
  api_key: bitrouter-local
  api_mode: chat_completions
  default_headers:
    x-bitrouter-workflow-session: "{session_id}"
agent:
  max_turns: 2
terminal:
  backend: local
toolsets:
  - hermes-cli
"#
        ),
    )
    .expect("write temporary Hermes config");

    std::fs::write(path.path().join(".env"), "OPENAI_API_KEY=bitrouter-local\n")
        .expect("write temporary Hermes env");

    path
}

fn config_for_real_agent(upstream: &str) -> bitrouter_sdk::config::Config {
    let yaml = format!(
        r#"
server:
  listen: "127.0.0.1:0"
  skip_auth: true
database:
  url: "sqlite::memory:"
providers:
  mock_chat:
    api_base: {upstream}
    api_key: test-key
    api_protocol:
      - "*": chat_completions
    models:
      - id: test-chat
        pricing:
          input_micro_usd_per_token: 1.0
          output_micro_usd_per_token: 2.0
  mock_responses:
    api_base: {upstream}
    api_key: test-key
    api_protocol:
      - "*": responses
    models:
      - id: test-responses
        pricing:
          input_micro_usd_per_token: 1.0
          output_micro_usd_per_token: 2.0
  mock_messages:
    api_base: {upstream}
    api_key: test-key
    api_protocol:
      - "*": messages
    models:
      - id: test-messages
        pricing:
          input_micro_usd_per_token: 1.0
          output_micro_usd_per_token: 2.0
models:
  openai/bitrouter-hermes-tbench:
    strategy: priority
    endpoints:
      - provider: mock_chat
        service_id: test-chat
  openai/bitrouter-codex-e2e:
    strategy: priority
    endpoints:
      - provider: mock_responses
        service_id: test-responses
  anthropic/bitrouter-claude-e2e:
    strategy: priority
    endpoints:
      - provider: mock_messages
        service_id: test-messages
"#
    );
    bitrouter_sdk::config::parse_with(&yaml, |_| None).expect("config parses")
}

async fn serve_router(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind real-agent e2e server");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), task)
}
