//! Integration tests for `bitrouter acp serve|prompt`.
//!
//! Test 1 (`prompt_ndjson`) — in-process: build a `Config` with a bash ACP
//! stub agent, call [`bitrouter::acp_cli::prompt`] with a `Vec<u8>` sink,
//! parse the NDJSON output, and assert that:
//!   - at least one `session_update` line with `text: "hi"` is emitted, and
//!   - the final line is `{"type":"result","stop_reason":"EndTurn"}`.
//!
//! Test 2 (`serve_subprocess_e2e`) — subprocess: write a temp config YAML,
//! spawn `bitrouter acp serve --agent stub --config <path>` as a child
//! process, drive its stdio with raw JSON-RPC NDJSON (the ACP wire format),
//! and assert the full `initialize` → `session/new` → `session/prompt` round-
//! trip succeeds, including the forwarded `session/update` carrying "hi".

#![cfg(unix)] // bash stubs are Unix-only

use std::collections::HashMap;

use bitrouter_sdk::acp::transport::{AcpAgentConfig, AcpTransport};
use bitrouter_sdk::config::Config;

/// Bash ACP stub: initialize → session/new → prompt emits one update then
/// end_turn. Identical to the stubs used in the substrate engine/down tests.
const BASH_STUB: &str = r#"
    while read line; do
      id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      case "$line" in
        *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
        *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
        *session/prompt*) printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}\n';
                          printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
      esac
    done
"#;

/// Build a `Config` whose `agents` map has one stub agent backed by the bash
/// stub above. All other config fields are at their defaults.
fn stub_config() -> Config {
    let agent_cfg = AcpAgentConfig {
        name: "stub".to_string(),
        transport: AcpTransport::Stdio {
            command: "bash".to_string(),
            args: vec!["-c".to_string(), BASH_STUB.to_string()],
            env: HashMap::new(),
        },
    };
    let mut cfg = Config::default();
    cfg.agents.insert("stub".to_string(), agent_cfg);
    cfg
}

// ── Test 1: NDJSON prompt (in-process) ───────────────────────────────────────

/// Call `acp_cli::prompt` with a `Vec<u8>` sink, parse the NDJSON output, and
/// assert the expected lines appear.
///
/// The test temporarily changes the working directory to a temp dir so
/// `Session::launch` (which calls `current_dir()` internally) finds a valid
/// base path.
#[tokio::test]
async fn prompt_ndjson() {
    let base = tempfile::tempdir().expect("tempdir");

    // Change cwd to the temp dir; restore on exit. `set_current_dir` is
    // process-global, but each nextest test runs in its own process, so this
    // does not race other tests under the default `cargo nextest` runner.
    let orig_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(base.path()).expect("set_current_dir");

    let source = bitrouter::paths::ConfigSource::Default {
        home: base.path().to_path_buf(),
    };
    let mut buf: Vec<u8> = Vec::new();
    let ctx = bitrouter::acp_cli::SpawnContext {
        source: &source,
        config: stub_config(),
        agent_id: "stub",
        options: bitrouter::acp_cli::launch_options(None),
        routing: bitrouter::acp_cli::RoutingOptions {
            direct: true,
            ..Default::default()
        },
    };
    let result = bitrouter::acp_cli::prompt(ctx, "hello", false, None, &mut buf).await;

    let _ = std::env::set_current_dir(&orig_dir);

    result.expect("acp_cli::prompt should succeed");

    let output = String::from_utf8(buf).expect("valid utf8");
    let lines: Vec<&str> = output.lines().collect();

    assert!(!lines.is_empty(), "expected at least one NDJSON line");

    // The first line correlates this session's record for the orchestrator.
    // Running direct (the stub is not catalog-matched), so `via` is null.
    let first: serde_json::Value =
        serde_json::from_str(lines[0]).expect("first line must be valid JSON");
    assert_eq!(
        first.get("type").and_then(|t| t.as_str()),
        Some("session"),
        "first NDJSON line must be the session line; got: {}",
        lines[0]
    );
    // Contract break, `docs/ACP_CONTROLLER_AMENDMENT_1.md` §2: the minted
    // `record_id` alias is gone from the wire. Session identity is
    // harness-native, so the correlation line carries the id the harness
    // itself minted plus the controller instance the daemon meters by — the
    // two columns a spend query actually joins on.
    assert!(
        first.get("record_id").is_none(),
        "the manager-facing record_id alias is off the wire: {}",
        lines[0]
    );
    assert_eq!(
        first.get("session_id").and_then(|r| r.as_str()),
        Some("u1"),
        "session line must carry the harness-native session id: {}",
        lines[0]
    );
    // Not `is_some()`: `Value::get` on a JSON `null` returns `Some(Null)`, so
    // that spelling passes for a field that is present and empty — which is
    // what the previous assertion actually checked.
    assert!(
        first.get("controller_instance_id").is_none(),
        "the controller id is a claimed header, not the spend key on this \
         path, and must not be reported as though it joined: {}",
        lines[0]
    );
    assert!(
        first.get("launch_id").is_some(),
        "launch_id is what attributes a prompt session's spend: {}",
        lines[0]
    );
    assert!(
        first.get("via").map(|v| v.is_null()).unwrap_or(false),
        "direct session must report via=null: {}",
        lines[0]
    );

    // At least one line should be a message_chunk with the agent's "hi" text.
    // The NDJSON format uses the SessionUpdateKind variant name as the `type`
    // field (snake_case), so agent_message_chunk → "message_chunk".
    let has_hi = lines.iter().any(|line| {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            v.get("type").and_then(|t| t.as_str()) == Some("message_chunk")
                && v.get("text")
                    .and_then(|t| t.as_str())
                    .map(|t| t.contains("hi"))
                    .unwrap_or(false)
        } else {
            false
        }
    });
    assert!(
        has_hi,
        "expected a message_chunk NDJSON line with text 'hi'; output:\n{output}"
    );

    // The last line must be the result line with the ACP wire `stop_reason`.
    // The format uses serde's snake_case spelling, so EndTurn → "end_turn".
    let last_line = lines.last().expect("at least one line");
    let last: serde_json::Value =
        serde_json::from_str(last_line).expect("last line must be valid JSON");
    assert_eq!(
        last.get("type").and_then(|t| t.as_str()),
        Some("result"),
        "last NDJSON line must have type=result; got: {last_line}"
    );
    let stop_reason = last
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .expect("result line must have stop_reason");
    assert_eq!(
        stop_reason, "end_turn",
        "expected snake_case end_turn stop_reason, got: {stop_reason}"
    );
    // Bare `spawn -p` (no --result-schema) stays byte-compatible: the result
    // line must not grow contract fields.
    for key in ["result", "schema_ok", "raw"] {
        assert!(
            last.get(key).is_none(),
            "bare result line must not carry `{key}`: {last_line}"
        );
    }
}

// ── Test 1a: --result-schema contract ─────────────────────────────────────────

/// Bash stub whose FIRST prompt reply violates the schema (`"ok": 3`) and
/// whose second (the repair re-prompt) satisfies it — exercising the
/// one-repair loop end to end.
const REPAIR_STUB: &str = r#"
    n=0
    while read line; do
      id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      case "$line" in
        *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
        *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
        *session/prompt*)
          n=$((n+1))
          if [ "$n" = 1 ]; then
            printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"```json\\n{\\"ok\\": 3}\\n```"}}}}\n'
          else
            printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"```json\\n{\\"ok\\": true}\\n```"}}}}\n'
          fi
          printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
      esac
    done
"#;

/// Stub that never produces JSON at all — both attempts fail, so the result
/// line must report `schema_ok:false` + `result:null` + the raw text.
const HOPELESS_STUB: &str = r#"
    while read line; do
      id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      case "$line" in
        *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
        *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
        *session/prompt*) printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"words, not JSON"}}}}\n';
                          printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
      esac
    done
"#;

fn stub_config_with(script: &str) -> Config {
    let agent_cfg = AcpAgentConfig {
        name: "stub".to_string(),
        transport: AcpTransport::Stdio {
            command: "bash".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: HashMap::new(),
        },
    };
    let mut cfg = Config::default();
    cfg.agents.insert("stub".to_string(), agent_cfg);
    cfg
}

const OK_SCHEMA: &str =
    r#"{"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}"#;

/// Run a `--result-schema` prompt against `script` and return the parsed
/// terminal result line.
async fn result_line_for(script: &str) -> serde_json::Value {
    let base = tempfile::tempdir().expect("tempdir");
    let orig_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(base.path()).expect("set_current_dir");

    let source = bitrouter::paths::ConfigSource::Default {
        home: base.path().to_path_buf(),
    };
    let mut buf: Vec<u8> = Vec::new();
    let ctx = bitrouter::acp_cli::SpawnContext {
        source: &source,
        config: stub_config_with(script),
        agent_id: "stub",
        options: bitrouter::acp_cli::launch_options(None),
        routing: bitrouter::acp_cli::RoutingOptions {
            direct: true,
            ..Default::default()
        },
    };
    let contract =
        bitrouter::result_contract::ResultContract::from_flag(OK_SCHEMA).expect("valid schema");
    let result =
        bitrouter::acp_cli::prompt(ctx, "do the task", false, Some(contract), &mut buf).await;
    let _ = std::env::set_current_dir(&orig_dir);
    result.expect("prompt should succeed");

    let output = String::from_utf8(buf).expect("valid utf8");
    let last = output.lines().last().expect("at least one line");
    serde_json::from_str(last).expect("terminal line is JSON")
}

#[tokio::test]
async fn result_schema_repair_reprompt_recovers() {
    let last = result_line_for(REPAIR_STUB).await;
    assert_eq!(last["type"], "result");
    assert_eq!(
        last["schema_ok"], true,
        "the repair re-prompt's valid reply must be accepted: {last}"
    );
    assert_eq!(last["result"], serde_json::json!({"ok": true}));
    assert!(last.get("raw").is_none(), "no raw text on success: {last}");
}

#[tokio::test]
async fn result_schema_failure_reports_raw_and_never_blocks() {
    let last = result_line_for(HOPELESS_STUB).await;
    assert_eq!(last["type"], "result");
    assert_eq!(last["schema_ok"], false);
    assert!(
        last["result"].is_null(),
        "result is null on failure: {last}"
    );
    assert!(
        last["raw"]
            .as_str()
            .is_some_and(|r| r.contains("words, not JSON")),
        "raw reply text surfaces so the orchestrator is never blocked: {last}"
    );
}

// ── Test 1b: routing fail-fast (no session side effects) ─────────────────────

/// `apply_routing` for a catalog harness with an unreachable daemon must fail
/// fast with `DaemonUnreachable` — before any session is launched. It may
/// synthesize the catalog invocation into the config (so a later launch is
/// *possible*), but it creates no session record itself.
#[tokio::test]
async fn routing_fails_fast_on_dead_daemon() {
    let base = tempfile::tempdir().expect("tempdir");
    let source = bitrouter::paths::ConfigSource::Default {
        home: base.path().to_path_buf(),
    };
    let mut cfg = Config::default();
    // Isolate the daemon check from the auth check: with skip_auth the
    // credential resolves to the placeholder regardless of the environment.
    cfg.server.skip_auth = true;

    let opts = bitrouter::acp_cli::RoutingOptions {
        direct: false,
        // Loopback discard port — nothing serves `/health`, so the liveness
        // probe fails deterministically. `base_url` set means no auto-start.
        base_url: Some("http://127.0.0.1:9".to_string()),
        model: None,
        no_start: true,
    };

    let res = bitrouter::acp_cli::apply_routing(&source, &mut cfg, "claude-acp", &opts).await;
    assert!(
        matches!(
            res,
            Err(bitrouter::acp_cli::RoutingError::DaemonUnreachable { .. })
        ),
        "expected DaemonUnreachable, got: {res:?}"
    );
    // The catalog invocation was synthesized, but no session artifacts exist.
    assert!(cfg.agents.contains_key("claude-acp"));
    assert!(
        !base.path().join(".bitrouter").exists(),
        "fail-fast must not create session side effects"
    );
}

/// `--direct` skips routing entirely: no daemon probe, `via` is `None`, and a
/// catalog id is still synthesized so the session can launch.
#[tokio::test]
async fn routing_direct_skips_daemon_and_reports_no_via() {
    let base = tempfile::tempdir().expect("tempdir");
    let source = bitrouter::paths::ConfigSource::Default {
        home: base.path().to_path_buf(),
    };
    let mut cfg = Config::default();
    let opts = bitrouter::acp_cli::RoutingOptions {
        direct: true,
        base_url: Some("http://127.0.0.1:9".to_string()),
        model: None,
        no_start: true,
    };
    let routed = bitrouter::acp_cli::apply_routing(&source, &mut cfg, "claude-acp", &opts)
        .await
        .expect("direct routing never fails");
    assert!(routed.via.is_none(), "direct → no via");
    assert!(
        routed.launch_id.is_none(),
        "a direct session sends nothing through the daemon, so there is \
         nothing to attribute"
    );
    // The claude-acp invocation is available even though routing was skipped.
    assert!(cfg.agents.contains_key("claude-acp"));
}

#[tokio::test]
async fn routing_returns_and_applies_one_endpoint_plan() -> anyhow::Result<()> {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let daemon = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&daemon)
        .await;
    let base = tempfile::tempdir()?;
    let source = bitrouter::paths::ConfigSource::Default {
        home: base.path().to_path_buf(),
    };
    let mut config = Config::default();
    config.server.skip_auth = true;
    let options = bitrouter::acp_cli::RoutingOptions {
        direct: false,
        base_url: Some(daemon.uri()),
        model: Some("logical/model".to_string()),
        no_start: true,
    };

    let routed =
        bitrouter::acp_cli::apply_routing(&source, &mut config, "claude-acp", &options).await?;
    let plan = routed
        .endpoint_plan
        .ok_or_else(|| anyhow::anyhow!("routing did not return an endpoint plan"))?;
    let controller_id = routed
        .controller_instance_id
        .ok_or_else(|| anyhow::anyhow!("routing did not return a controller id"))?;
    assert_eq!(
        plan.headers
            .get("x-bitrouter-controller-id")
            .map(String::as_str),
        Some(controller_id.as_str())
    );
    assert_eq!(plan.model.as_deref(), Some("logical/model"));

    let entry = config
        .agents
        .get("claude-acp")
        .ok_or_else(|| anyhow::anyhow!("catalog agent was not synthesized"))?;
    let AcpTransport::Stdio { args, env, .. } = &entry.transport;
    assert_eq!(
        args,
        &["-y", "@agentclientprotocol/claude-agent-acp@0.70.0"]
    );
    assert_eq!(env.get("ANTHROPIC_BASE_URL"), Some(&daemon.uri()));
    assert_eq!(
        env.get("ANTHROPIC_MODEL").map(String::as_str),
        Some("logical/model")
    );
    let custom = env
        .get("ANTHROPIC_CUSTOM_HEADERS")
        .ok_or_else(|| anyhow::anyhow!("static headers were not applied"))?;
    assert!(custom.contains(&controller_id));
    Ok(())
}

#[tokio::test]
async fn unpinned_codex_acp_never_receives_cli_config_arguments() -> anyhow::Result<()> {
    let base = tempfile::tempdir()?;
    let source = bitrouter::paths::ConfigSource::Default {
        home: base.path().to_path_buf(),
    };
    let mut config = Config::default();
    config.agents.insert(
        "codex-custom".to_string(),
        AcpAgentConfig {
            name: "codex-custom".to_string(),
            transport: AcpTransport::Stdio {
                command: "npx".to_string(),
                args: vec![
                    "-y".to_string(),
                    "@agentclientprotocol/codex-acp@1.6.0".to_string(),
                ],
                env: HashMap::new(),
            },
        },
    );
    let options = bitrouter::acp_cli::RoutingOptions {
        direct: false,
        base_url: Some("http://127.0.0.1:9".to_string()),
        model: Some("logical/model".to_string()),
        no_start: true,
    };

    let routed =
        bitrouter::acp_cli::apply_routing(&source, &mut config, "codex-custom", &options).await?;
    assert!(routed.via.is_none());
    assert!(routed.endpoint_plan.is_none());
    let entry = config
        .agents
        .get("codex-custom")
        .ok_or_else(|| anyhow::anyhow!("custom Codex entry disappeared"))?;
    let AcpTransport::Stdio { args, env, .. } = &entry.transport;
    assert_eq!(args, &["-y", "@agentclientprotocol/codex-acp@1.6.0"]);
    assert!(env.is_empty());
    Ok(())
}

// ── shared raw JSON-RPC helpers (subprocess / socket e2e) ────────────────────

/// Send a JSON-RPC request line and read back lines until one matches the
/// given id (the response). Lines that don't match the id are collected as
/// notifications or intermediary messages.
async fn rpc_round_trip(
    writer: &mut (impl tokio::io::AsyncWriteExt + Unpin),
    reader: &mut tokio::io::BufReader<impl tokio::io::AsyncRead + Unpin>,
    request: serde_json::Value,
    request_id: &str,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    use tokio::io::AsyncBufReadExt;

    let line = serde_json::to_string(&request).expect("serialize request") + "\n";
    writer
        .write_all(line.as_bytes())
        .await
        .expect("write request");
    writer.flush().await.expect("flush");

    let mut notifications = Vec::new();
    loop {
        let mut buf = String::new();
        let n = reader
            .read_line(&mut buf)
            .await
            .expect("read response line");
        assert!(n > 0, "EOF before receiving response for id {request_id}");
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            // Skip blank lines (the ACP wire format is newline-delimited).
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("invalid JSON from server: {e}\nraw line: {trimmed:?}"));
        if v.get("id").and_then(|i| i.as_str()) == Some(request_id) {
            return (v, notifications);
        }
        // This is a notification (no matching id); collect it.
        notifications.push(v);
    }
}

/// Run one round-trip under `timeout`; panic on elapse so a stalled server
/// never hangs the test runner.
async fn bounded_round_trip(
    writer: &mut (impl tokio::io::AsyncWriteExt + Unpin),
    reader: &mut tokio::io::BufReader<impl tokio::io::AsyncRead + Unpin>,
    request: serde_json::Value,
    request_id: &str,
    timeout: std::time::Duration,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    match tokio::time::timeout(timeout, rpc_round_trip(writer, reader, request, request_id)).await {
        Ok(out) => out,
        Err(_) => panic!(
            "timed out after {}s waiting for response to id {request_id}",
            timeout.as_secs()
        ),
    }
}

// ── Test 2: serve subprocess E2E ─────────────────────────────────────────────

/// A minimal YAML config for the subprocess serve test.
/// Uses a block-scalar literal (`|`) for the bash script, which avoids any
/// quoting issues. The script is the same ACP stub as `BASH_STUB` but
/// written as a YAML literal block.
const SERVE_CONFIG_YAML: &str = r#"
agents:
  stub:
    name: stub
    transport:
      type: stdio
      command: bash
      args:
        - "-c"
        - |
            session_count=0
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"list":{},"resume":{}}},"agentInfo":{"name":"stub-harness","version":"1.0.0"}}}\n' "$id";;
                *session/new*)  session_count=$((session_count+1));
                                if [ "$session_count" = 1 ]; then native="native-a"; else native="native-b"; fi;
                                printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"%s"}}\n' "$id" "$native";;
                *session/load*) printf '{"jsonrpc":"2.0","id":"%s","result":{"_meta":{"loadedBy":"harness"}}}\n' "$id";;
                *session/prompt*) printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"native-b","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}\n';
                                  printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
              esac
            done
"#;

/// Spawn `bitrouter acp serve --agent stub --config <path>` as a child process
/// and drive it with raw JSON-RPC NDJSON — the actual ACP wire format over
/// stdio. This exercises the path that the in-process `down.rs` duplex tests
/// cannot: real OS-level stdio pipes and the CLI entry point.
///
/// The test sends `initialize` → two `session/new` calls → `session/load` →
/// `session/prompt` and asserts:
/// - each request receives its JSON-RPC response, and
/// - every session ID is the harness-authored native ID, and
/// - the forwarded `session/update` containing "hi" arrives before the prompt
///   response.
///
/// Every request/response round-trip is bounded by [`RPC_TIMEOUT`] so a child
/// crash or stall fails the test promptly instead of hanging CI. A
/// `multi_thread` runtime is used so the timeout timer fires even while the
/// blocking child-stdio read is pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_subprocess_e2e() {
    use std::time::Duration;

    use tokio::io::BufReader;

    /// Per-round-trip timeout — generous enough for a debug-build spawn + ACP
    /// handshake, tight enough to fail fast on a stalled child.
    const RPC_TIMEOUT: Duration = Duration::from_secs(10);

    // Write the config YAML to a temp file.
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("bitrouter.yaml");
    std::fs::write(&config_path, SERVE_CONFIG_YAML).expect("write config");

    // Locate the built binary via CARGO_MANIFEST_DIR → workspace target dir.
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest.ancestors().nth(2).expect("workspace root");
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let binary = workspace_root
        .join("target")
        .join(profile)
        .join("bitrouter");

    if !binary.exists() {
        eprintln!(
            "serve_subprocess_e2e: binary not found at {}; skipping",
            binary.display()
        );
        return;
    }

    // Spawn `bitrouter acp serve --agent stub --config <path>`.
    // Redirect stderr to a temp file so we can inspect it on failure.
    let stderr_path = dir.path().join("serve.stderr");
    let stderr_file = std::fs::File::create(&stderr_path).expect("stderr file");
    let mut child = tokio::process::Command::new(&binary)
        .args([
            "acp",
            "serve",
            "--agent",
            "stub",
            "--config",
            config_path.to_str().expect("config path utf8"),
        ])
        // The substrate roots its session records at the cwd;
        // pin it to the tempdir so test artifacts never land in the repo.
        .current_dir(dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(stderr_file)
        // Kill the child if this test panics (e.g. on a round-trip timeout) so
        // a stalled server is reaped rather than leaked.
        .kill_on_drop(true)
        .spawn()
        .expect("spawn bitrouter acp serve");

    let mut child_stdin = child.stdin.take().expect("child stdin");
    let child_stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(child_stdout);

    // ── 1. initialize ─────────────────────────────────────────────────────
    let (init_resp, _) = bounded_round_trip(
        &mut child_stdin,
        &mut reader,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "initialize",
            "params": { "protocolVersion": 1 }
        }),
        "1",
        RPC_TIMEOUT,
    )
    .await;
    assert!(
        init_resp.get("result").is_some(),
        "initialize must return a result; got: {init_resp}"
    );
    assert_eq!(
        init_resp["result"]["agentInfo"]["name"],
        "bitrouter-acp-controller"
    );
    assert!(init_resp["result"]["agentCapabilities"]["providers"].is_null());
    assert_eq!(
        init_resp["result"]["_meta"]["bitrouter.dev/controller"]["upstreamAgentInfo"]["name"],
        "stub-harness"
    );

    // ── 2. session/new ────────────────────────────────────────────────────
    let (new_resp, _) = bounded_round_trip(
        &mut child_stdin,
        &mut reader,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": "2",
            "method": "session/new",
            "params": { "cwd": "/", "mcpServers": [] }
        }),
        "2",
        RPC_TIMEOUT,
    )
    .await;
    let first_session_id = new_resp["result"]["sessionId"]
        .as_str()
        .expect("session/new must return sessionId");
    assert_eq!(first_session_id, "native-a");
    assert!(new_resp["result"].get("record_id").is_none());

    let (second_new_resp, _) = bounded_round_trip(
        &mut child_stdin,
        &mut reader,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": "3",
            "method": "session/new",
            "params": { "cwd": "/", "mcpServers": [] }
        }),
        "3",
        RPC_TIMEOUT,
    )
    .await;
    let second_session_id = second_new_resp["result"]["sessionId"].clone();
    assert_eq!(second_session_id, "native-b");

    let (load_resp, _) = bounded_round_trip(
        &mut child_stdin,
        &mut reader,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": "4",
            "method": "session/load",
            "params": { "sessionId": "native-a", "cwd": "/", "mcpServers": [] }
        }),
        "4",
        RPC_TIMEOUT,
    )
    .await;
    assert_eq!(load_resp["result"]["_meta"]["loadedBy"], "harness");

    // ── 5. session/prompt ─────────────────────────────────────────────────
    // The stub streams a `session/update` before the prompt result. Collect
    // all lines until we get the response for id "5".
    let (prompt_resp, notifications) = bounded_round_trip(
        &mut child_stdin,
        &mut reader,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": "5",
            "method": "session/prompt",
            "params": {
                "sessionId": second_session_id,
                "prompt": [{ "type": "text", "text": "do X" }]
            }
        }),
        "5",
        RPC_TIMEOUT,
    )
    .await;

    let stop_reason = prompt_resp["result"]["stopReason"]
        .as_str()
        .expect("session/prompt must return stopReason");
    assert_eq!(stop_reason, "end_turn", "expected end_turn stop reason");

    // The stub emits a `session/update` notification with "hi"; assert it
    // was forwarded through the serve pipeline to our client.
    let has_hi = notifications.iter().any(|n| {
        n.get("method").and_then(|m| m.as_str()) == Some("session/update")
            && n["params"]["sessionId"] == "native-b"
            && format!("{n}").contains("hi")
    });
    assert!(
        has_hi,
        "expected a forwarded session/update with 'hi'; notifications: {notifications:?}"
    );

    // ── Disconnect: serve must exit on its OWN when the manager closes stdin ─
    // This is the regression guard for the process-leak bug: dropping the
    // child's stdin handle delivers EOF to `bitrouter acp serve` (the manager
    // disconnecting). The server must detect EOF, tear down, drop its
    // `Arc<Session>` (which kills the upstream agent child), and exit — WITHOUT
    // us having to `kill()` it. We assert it exits on its own within a few
    // seconds.
    drop(child_stdin);

    let exit_status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    match exit_status {
        Ok(Ok(status)) => {
            // Exited on its own. Success is exiting promptly; the exit code may be
            // non-zero because `connect_with` surfaces the EOF as an error, which
            // is fine — the point is it did not hang and did not need a kill.
            eprintln!("serve exited on stdin close with status: {status:?}");
        }
        Ok(Err(e)) => panic!("error waiting for serve child: {e}"),
        Err(_) => {
            // Hung: kill so the test runner isn't left with a leaked process,
            // then fail loudly — this is the bug we are guarding against.
            let _ = child.kill().await;
            panic!(
                "bitrouter acp serve did NOT exit within 5s after the manager \
                 closed stdin — it hung (process/agent-child leak regression)"
            );
        }
    }
}

/// Regression: a permission request during a **headless** `acp prompt` must
/// be auto-denied so the turn completes — with no manager attached, an
/// unconsumed `session/request_permission` would otherwise park its resolver
/// forever and hang the process (found driving a real agent that asked for
/// file-write permission).
#[tokio::test]
async fn prompt_headless_denies_permission_and_completes() {
    const PERM_STUB: &str = r#"
        while read line; do
          id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
          case "$line" in
            *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
            *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
            *session/prompt*)
                printf '{"jsonrpc":"2.0","id":"99","method":"session/request_permission","params":{"sessionId":"u1","toolCall":{"toolCallId":"tc1","title":"write file"},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"rej","name":"Reject","kind":"reject_once"}]}}\n'
                read resp
                chosen=$(echo "$resp" | sed -n 's/.*"optionId":"\([^"]*\)".*/\1/p')
                printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"chose:%s"}}}}\n' "$chosen"
                printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
          esac
        done
    "#;
    let agent_cfg = AcpAgentConfig {
        name: "perm-stub".to_string(),
        transport: AcpTransport::Stdio {
            command: "bash".to_string(),
            args: vec!["-c".to_string(), PERM_STUB.to_string()],
            env: HashMap::new(),
        },
    };
    let mut cfg = Config::default();
    cfg.agents.insert("perm-stub".to_string(), agent_cfg);

    let base = tempfile::tempdir().expect("tempdir");
    let orig_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(base.path()).expect("set_current_dir");

    let source = bitrouter::paths::ConfigSource::Default {
        home: base.path().to_path_buf(),
    };
    let mut buf: Vec<u8> = Vec::new();
    let ctx = bitrouter::acp_cli::SpawnContext {
        source: &source,
        config: cfg,
        agent_id: "perm-stub",
        options: bitrouter::acp_cli::launch_options(None),
        routing: bitrouter::acp_cli::RoutingOptions {
            direct: true,
            ..Default::default()
        },
    };
    // Bound the whole run: before the fix this hung forever.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        bitrouter::acp_cli::prompt(ctx, "write it", false, None, &mut buf),
    )
    .await;

    let _ = std::env::set_current_dir(&orig_dir);

    let result = result.expect("headless prompt must not hang on a permission request");
    result.expect("prompt should complete");

    let output = String::from_utf8(buf).expect("utf8");
    assert!(
        output.contains("chose:rej"),
        "the permission must be auto-denied (reject option); output:\n{output}"
    );
    assert!(
        output.contains("\"result\""),
        "turn must complete:\n{output}"
    );
}

/// I8: a turn that blows `--turn-timeout` is cancelled cooperatively and then
/// failed. The stub answers the handshake and `session/new`, acknowledges the
/// `session/cancel` by doing nothing, and never answers `session/prompt` — so
/// only the client's own deadline can end the turn.
#[tokio::test]
async fn prompt_turn_timeout_fails_the_turn_instead_of_hanging() {
    const STALL_STUB: &str = r#"
        while read line; do
          id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
          case "$line" in
            *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
            *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
          esac
        done
    "#;
    let base = tempfile::tempdir().expect("tempdir");
    let orig_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(base.path()).expect("set_current_dir");

    let source = bitrouter::paths::ConfigSource::Default {
        home: base.path().to_path_buf(),
    };
    let mut buf: Vec<u8> = Vec::new();
    let ctx = bitrouter::acp_cli::SpawnContext {
        source: &source,
        config: stub_config_with(STALL_STUB),
        agent_id: "stub",
        // One second, plus the client's three-second cooperative-cancel grace.
        options: bitrouter::acp_cli::launch_options(Some(1)),
        routing: bitrouter::acp_cli::RoutingOptions {
            direct: true,
            ..Default::default()
        },
    };
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        bitrouter::acp_cli::prompt(ctx, "hello", false, None, &mut buf),
    )
    .await;
    let _ = std::env::set_current_dir(&orig_dir);

    let result = outcome.expect("--turn-timeout must end the turn, not hang the process");
    let error = format!("{:#}", result.expect_err("a stalled turn must fail"));
    assert!(
        error.contains("timed out"),
        "the failure must name the deadline: {error}"
    );
}

/// I11: a harness that dies mid-prompt fails the turn rather than hanging it.
/// The stub answers the handshake and `session/new`, then exits with the
/// prompt in flight.
#[tokio::test]
async fn prompt_fails_fast_when_the_harness_dies_mid_turn() {
    const DYING_STUB: &str = r#"
        while read line; do
          id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
          case "$line" in
            *initialize*)     printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
            *session/new*)    printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
            *session/prompt*) exit 0;;
          esac
        done
    "#;
    let base = tempfile::tempdir().expect("tempdir");
    let orig_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(base.path()).expect("set_current_dir");

    let source = bitrouter::paths::ConfigSource::Default {
        home: base.path().to_path_buf(),
    };
    let mut buf: Vec<u8> = Vec::new();
    let ctx = bitrouter::acp_cli::SpawnContext {
        source: &source,
        config: stub_config_with(DYING_STUB),
        agent_id: "stub",
        options: bitrouter::acp_cli::launch_options(None),
        routing: bitrouter::acp_cli::RoutingOptions {
            direct: true,
            ..Default::default()
        },
    };
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        bitrouter::acp_cli::prompt(ctx, "hello", false, None, &mut buf),
    )
    .await;
    let _ = std::env::set_current_dir(&orig_dir);

    let result = outcome.expect("a dead harness must fail the turn, not hang it");
    assert!(
        result.is_err(),
        "a turn whose harness died must fail; output:\n{}",
        String::from_utf8_lossy(&buf)
    );
}

// ── Test 3: forwarded update variants ───────────────────────────────────────

/// A stub that emits, during one prompt turn, every stable v1 `session/update`
/// the gateway used to swallow — then ends the turn.
const CONFORMANCE_CONFIG_YAML: &str = r#"
agents:
  stub:
    name: stub
    transport:
      type: stdio
      command: bash
      args:
        - "-c"
        - |
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
                *session/prompt*)
                  printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"plan","entries":[{"content":"step one","priority":"high","status":"pending"}]}}}\n';
                  printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"create_plan","description":"draft a plan"}]}}}\n';
                  printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"current_mode_update","currentModeId":"plan"}}}\n';
                  printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"config_option_update","configOptions":[]}}}\n';
                  printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"session_info_update","title":"conformance"}}}\n';
                  printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
              esac
            done
"#;

/// A live `bitrouter acp serve` subprocess, initialized and with a session
/// open — the fixture the four conformance assertions below each drive.
struct ServeFixture {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    reader: tokio::io::BufReader<tokio::process::ChildStdout>,
    session_id: serde_json::Value,
    _dir: tempfile::TempDir,
}

/// Per-round-trip timeout — generous enough for a debug-build spawn plus the
/// ACP handshake, tight enough to fail fast on a stalled child.
const CONFORMANCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

impl ServeFixture {
    /// Spawn the subprocess and complete `initialize` → `session/new`.
    /// `None` when the binary has not been built (the suite is still useful
    /// without it, and a missing binary is not a protocol failure).
    async fn launch() -> Option<Self> {
        use tokio::io::BufReader;

        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("bitrouter.yaml");
        assert!(
            std::fs::write(&config_path, CONFORMANCE_CONFIG_YAML).is_ok(),
            "write config"
        );

        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest.ancestors().nth(2).expect("workspace root");
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let binary = workspace_root
            .join("target")
            .join(profile)
            .join("bitrouter");
        if !binary.exists() {
            eprintln!(
                "conformance: binary not found at {}; skipping",
                binary.display()
            );
            return None;
        }

        let stderr_path = dir.path().join("serve.stderr");
        let stderr_file = std::fs::File::create(&stderr_path).expect("stderr file");
        let mut child = tokio::process::Command::new(&binary)
            .args([
                "acp",
                "serve",
                "--agent",
                "stub",
                "--direct",
                "--config",
                config_path.to_str().expect("config path utf8"),
            ])
            .current_dir(dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(stderr_file)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn bitrouter acp serve");

        let mut stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let mut reader = BufReader::new(stdout);

        let (init, _) = bounded_round_trip(
            &mut stdin,
            &mut reader,
            serde_json::json!({"jsonrpc":"2.0","id":"1","method":"initialize",
                               "params":{"protocolVersion":1}}),
            "1",
            CONFORMANCE_TIMEOUT,
        )
        .await;
        assert!(init.get("result").is_some(), "initialize failed: {init}");

        let (new_resp, _) = bounded_round_trip(
            &mut stdin,
            &mut reader,
            serde_json::json!({"jsonrpc":"2.0","id":"2","method":"session/new",
                               "params":{"cwd":"/","mcpServers":[]}}),
            "2",
            CONFORMANCE_TIMEOUT,
        )
        .await;
        assert!(
            new_resp["result"]["sessionId"].is_string(),
            "session/new failed: {new_resp}"
        );

        Some(Self {
            child,
            stdin,
            reader,
            session_id: new_resp["result"]["sessionId"].clone(),
            _dir: dir,
        })
    }

    async fn call(
        &mut self,
        id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> (serde_json::Value, Vec<serde_json::Value>) {
        bounded_round_trip(
            &mut self.stdin,
            &mut self.reader,
            serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            id,
            CONFORMANCE_TIMEOUT,
        )
        .await
    }

    async fn prompt(&mut self, id: &str, text: &str) -> Vec<serde_json::Value> {
        let session = self.session_id.clone();
        let (resp, notifications) = self
            .call(
                id,
                "session/prompt",
                serde_json::json!({"sessionId":session,
                                   "prompt":[{"type":"text","text":text}]}),
            )
            .await;
        assert!(
            resp.get("result").is_some(),
            "session/prompt failed: {resp}"
        );
        notifications
    }

    /// Every `session/update` discriminant seen among `notifications`.
    fn update_kinds(notifications: &[serde_json::Value]) -> Vec<&str> {
        notifications
            .iter()
            .filter(|n| n["method"] == "session/update")
            .filter_map(|n| n["params"]["update"]["sessionUpdate"].as_str())
            .collect()
    }

    /// Shut the subprocess down the way a real manager does, and **reap it**.
    ///
    /// Dropping stdin is the designed teardown: the child sees EOF, `serve`
    /// returns, the session drops, and the agent grandchild is killed with it.
    /// A bare `kill()` skips all of that and races — the grandchild is
    /// orphaned and can outlive the test still holding inherited descriptors,
    /// which nextest reports as a "leaky" test. On a Linux CI runner that held
    /// pipe keeps the *step* alive long after the suite finishes, until the
    /// runner is killed for unresponsiveness and takes its log with it.
    async fn shutdown(self) {
        let Self {
            mut child, stdin, ..
        } = self;
        drop(stdin);
        match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                // Hung: kill *and* reap, so the failure is a failure rather
                // than a leak that outlives the run.
                let _ = child.kill().await;
                let _ = child.wait().await;
                panic!("bitrouter acp serve did not exit within 5s of stdin close");
            }
        }
    }
}

/// The five `session/update` variants the legacy single-session gateway used to
/// swallow survive the round-trip to the manager.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_forwarded_update_variants_survive_round_trip() {
    let Some(mut fixture) = ServeFixture::launch().await else {
        return;
    };
    let notifications = fixture.prompt("3", "go").await;
    let kinds = ServeFixture::update_kinds(&notifications);
    for expected in [
        "plan",
        "available_commands_update",
        "current_mode_update",
        "config_option_update",
        "session_info_update",
    ] {
        assert!(
            kinds.contains(&expected),
            "'{expected}' must survive the round-trip; saw {kinds:?}"
        );
    }
    fixture.shutdown().await;
}

// ── Test 5: `chat` on a pipe ─────────────────────────────────────────────────

/// `bitrouter chat` renders for a person; a redirect has none. Spawn it with
/// stdout on a pipe, feed it one prompt, and assert the transcript arrives as
/// plain text — **no ESC byte anywhere**.
///
/// This is the property, not a proxy for it: the interactive path's live row,
/// cursor moves, and raw-mode switches are all ESC sequences, so a single
/// `0x1b` in this output would mean the terminal path ran against a file.
///
/// Driven as a subprocess because that is the only way to get a pipe on
/// stdout: in-process, the test harness's own capture decides what
/// `is_terminal()` reports.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_on_a_pipe_is_plain_text() {
    use std::time::Duration;

    use tokio::io::AsyncWriteExt as _;

    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("bitrouter.yaml");
    std::fs::write(&config_path, SERVE_CONFIG_YAML).expect("write config");

    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest.ancestors().nth(2).expect("workspace root");
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let binary = workspace_root
        .join("target")
        .join(profile)
        .join("bitrouter");
    if !binary.exists() {
        eprintln!(
            "chat_on_a_pipe_is_plain_text: binary not found at {}; skipping",
            binary.display()
        );
        return;
    }

    let stderr_path = dir.path().join("chat.stderr");
    let stderr_file = std::fs::File::create(&stderr_path).expect("stderr file");
    let mut child = tokio::process::Command::new(&binary)
        .args([
            "chat",
            "stub",
            "--direct",
            "--config",
            config_path.to_str().expect("config path utf8"),
        ])
        .current_dir(dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(stderr_file)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn bitrouter chat");

    let mut child_stdin = child.stdin.take().expect("child stdin");
    child_stdin
        .write_all(b"hello\n")
        .await
        .expect("write the prompt");
    // Closing stdin is this path's Ctrl-D: the session ends and the process
    // exits, which is also what makes the test terminate.
    drop(child_stdin);

    let finished = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("chat must exit once stdin closes")
        .expect("chat output");
    let stdout = String::from_utf8_lossy(&finished.stdout).to_string();
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();

    assert!(
        !finished.stdout.contains(&0x1b),
        "a redirected stdout must carry no escape sequences; got {:?}\nstderr:\n{stderr}",
        stdout
    );
    assert!(
        stdout.contains("hi"),
        "the agent's reply must still reach a pipe; got {stdout:?}\nstderr:\n{stderr}"
    );
}
