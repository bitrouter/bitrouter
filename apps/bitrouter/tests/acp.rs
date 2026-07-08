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

use bitrouter_sdk::acp::{AcpAgentConfig, AcpTransport};
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
/// base path. No worktree is requested, so the git-repo check is skipped.
#[tokio::test]
async fn prompt_ndjson() {
    let base = tempfile::tempdir().expect("tempdir");

    // Change cwd to the temp dir; restore on exit. `set_current_dir` is
    // process-global, but each nextest test runs in its own process, so this
    // does not race other tests under the default `cargo nextest` runner.
    let orig_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(base.path()).expect("set_current_dir");

    let mut buf: Vec<u8> = Vec::new();
    let result = bitrouter::acp_cli::prompt(
        stub_config(),
        "stub",
        bitrouter::acp_cli::launch_options(None, false, false, None),
        "hello",
        false,
        &mut buf,
    )
    .await;

    let _ = std::env::set_current_dir(&orig_dir);

    result.expect("acp_cli::prompt should succeed");

    let output = String::from_utf8(buf).expect("valid utf8");
    let lines: Vec<&str> = output.lines().collect();

    assert!(!lines.is_empty(), "expected at least one NDJSON line");

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

/// Spawn `bitrouter acp serve --agent stub --config <path>` as a child process
/// and drive it with raw JSON-RPC NDJSON — the actual ACP wire format over
/// stdio. This exercises the path that the in-process `down.rs` duplex tests
/// cannot: real OS-level stdio pipes and the CLI entry point.
///
/// The test sends `initialize` → `session/new` → `session/prompt` and asserts:
/// - each request receives its JSON-RPC response, and
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

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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
        // The substrate roots its session records/transcripts at the cwd;
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

    /// Send a JSON-RPC request line to the child and read back lines until one
    /// matches the given id (the response). Lines that don't match the id are
    /// collected as notifications or intermediary messages.
    async fn rpc_round_trip(
        stdin: &mut (impl AsyncWriteExt + Unpin),
        reader: &mut BufReader<impl tokio::io::AsyncRead + Unpin>,
        request: serde_json::Value,
        request_id: &str,
    ) -> (serde_json::Value, Vec<serde_json::Value>) {
        let line = serde_json::to_string(&request).expect("serialize request") + "\n";
        stdin
            .write_all(line.as_bytes())
            .await
            .expect("write request");
        stdin.flush().await.expect("flush");

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

    // Run one round-trip under [`RPC_TIMEOUT`]; kill the child and panic on
    // elapse so a stalled server never hangs the test runner.
    async fn bounded_round_trip(
        stdin: &mut (impl AsyncWriteExt + Unpin),
        reader: &mut BufReader<impl tokio::io::AsyncRead + Unpin>,
        request: serde_json::Value,
        request_id: &str,
        timeout: Duration,
    ) -> (serde_json::Value, Vec<serde_json::Value>) {
        match tokio::time::timeout(timeout, rpc_round_trip(stdin, reader, request, request_id))
            .await
        {
            Ok(out) => out,
            Err(_) => panic!(
                "timed out after {}s waiting for response to id {request_id}",
                timeout.as_secs()
            ),
        }
    }

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
    let session_id = new_resp["result"]["sessionId"]
        .as_str()
        .expect("session/new must return sessionId");
    assert!(!session_id.is_empty(), "sessionId must not be empty");

    // ── 3. session/prompt ─────────────────────────────────────────────────
    // The stub streams a `session/update` before the prompt result. Collect
    // all lines until we get the response for id "3".
    let (prompt_resp, notifications) = bounded_round_trip(
        &mut child_stdin,
        &mut reader,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": "3",
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "do X" }]
            }
        }),
        "3",
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
