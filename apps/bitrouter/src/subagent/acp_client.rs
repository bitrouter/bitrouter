//! A minimal ACP client: spawns an agent, drives one prompt to completion, and
//! collects the final message, tool-call targets, and stop reason. Newline-
//! delimited JSON-RPC 2.0 over the child's stdio.

use std::collections::BTreeMap;
use std::process::Stdio;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use bitrouter_sdk::{BitrouterError, Result};

/// Environment variables passed through to the spawned worker. Deliberately a
/// short allowlist (not the daemon's full env) so upstream provider API keys are
/// never inherited — the worker reaches models only via its scoped `brvk_`.
const ENV_PASSTHROUGH: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "USER",
    "SHELL",
    "NODE_PATH",
    "NODE_OPTIONS",
];

/// Outcome of driving one ACP prompt.
#[derive(Debug, Clone, Default)]
pub struct SessionOutcome {
    /// Concatenated agent message text.
    pub final_message: String,
    /// Tool-call titles seen (proxy for files touched / actions).
    pub tool_calls: Vec<String>,
    /// The terminal `stopReason`, if the agent sent one.
    pub stop_reason: Option<String>,
}

/// How to launch the worker.
pub struct WorkerSpawn {
    /// Executable (e.g. `opencode`).
    pub command: String,
    /// Args (e.g. `["acp", "--cwd", "<abs>"]`).
    pub args: Vec<String>,
    /// Extra env (e.g. `OPENCODE_CONFIG`).
    pub env: BTreeMap<String, String>,
    /// Working directory for the child process. Set so harnesses that ignore an
    /// ACP `--cwd` (e.g. claude-agent-acp) still write relative paths into the
    /// isolated worktree rather than `$HOME`.
    pub working_dir: Option<String>,
}

/// Spawn the worker, run `initialize → session/new → session/prompt(task)`, and
/// collect the outcome. `kill_on_drop` guarantees teardown.
pub async fn drive_once(spawn: WorkerSpawn, task: &str) -> Result<SessionOutcome> {
    let mut cmd = Command::new(&spawn.command);
    // Security: do NOT inherit the daemon's environment — it holds upstream
    // provider API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …) the subagent
    // must never see (the spec's "no upstream key to the child" guarantee). The
    // worker reaches models ONLY through its scoped `brvk_` in `spawn.env`'s
    // `OPENCODE_CONFIG`. Pass through just the minimal vars the harness needs to
    // start (node/opencode lookup + locale), then the worker's own env.
    cmd.env_clear();
    for key in ENV_PASSTHROUGH {
        if let Some(val) = std::env::var_os(key) {
            cmd.env(key, val);
        }
    }
    if let Some(dir) = &spawn.working_dir {
        cmd.current_dir(dir);
    }
    cmd.args(&spawn.args)
        .envs(&spawn.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child: Child = cmd
        .spawn()
        .map_err(|e| BitrouterError::internal(format!("spawning '{}': {e}", spawn.command)))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| BitrouterError::internal("no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| BitrouterError::internal("no stdout"))?;
    let mut lines = BufReader::new(stdout).lines();

    // ACP `session/new` requires an ABSOLUTE cwd for some agents (claude-agent-acp
    // rejects a relative one; opencode tolerates "."). Use the worktree path.
    let session_cwd = spawn.working_dir.clone().unwrap_or_else(|| ".".to_string());

    let mut next_id = 1i64;
    send_request(
        &mut stdin,
        &mut next_id,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": { "readTextFile": true, "writeTextFile": true },
                "terminal": true
            }
        }),
    )
    .await?;
    wait_for_result(&mut lines, 1).await?;

    send_request(
        &mut stdin,
        &mut next_id,
        "session/new",
        json!({ "cwd": session_cwd, "mcpServers": [] }),
    )
    .await?;
    let new_res = wait_for_result(&mut lines, 2).await?;
    let session_id = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BitrouterError::internal("session/new: no sessionId"))?
        .to_string();

    send_request(
        &mut stdin,
        &mut next_id,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": task }]
        }),
    )
    .await?;

    let mut outcome = SessionOutcome::default();
    loop {
        let line = match lines
            .next_line()
            .await
            .map_err(|e| BitrouterError::internal(format!("read: {e}")))?
        {
            Some(l) => l,
            None => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Server-initiated request: has both id and method. Answer defensively
        // so the worker never blocks. `session/request_permission` needs a
        // proper "selected/allow" outcome (claude-agent-acp asks before running
        // tools; an empty `{}` reads as a denial → no work). opencode never
        // exercises this, but claude-acp does.
        if msg.get("id").is_some() && msg.get("method").is_some() {
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let result = if method == "session/request_permission" {
                let opts = msg["params"]["options"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let option_id = opts
                    .iter()
                    .find(|o| o["kind"].as_str() == Some("allow_always"))
                    .or_else(|| {
                        opts.iter()
                            .find(|o| o["kind"].as_str() == Some("allow_once"))
                    })
                    .or_else(|| opts.first())
                    .and_then(|o| o["optionId"].as_str())
                    .unwrap_or("allow")
                    .to_string();
                json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
            } else {
                json!({})
            };
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            let _ = stdin.write_all(format!("{}\n", reply).as_bytes()).await;
            continue;
        }
        // Response to our session/prompt request: has id, has result.
        if msg.get("id").is_some() && msg.get("result").is_some() {
            outcome.stop_reason = msg["result"]
                .get("stopReason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            break;
        }
        // Error response to our request (e.g. the worker's inference was DENIED
        // by the budget cap). Don't swallow it — surface it as the stop reason so
        // the fail-closed signal reaches the parent.
        if msg.get("id").is_some() && msg.get("error").is_some() {
            let emsg = msg["error"]
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("agent error");
            outcome.stop_reason = Some("error".to_string());
            if outcome.final_message.is_empty() {
                outcome.final_message = format!("agent error: {emsg}");
            }
            break;
        }
        // Notification: has method, no id.
        if msg.get("method").and_then(|m| m.as_str()) == Some("session/update") {
            let u = &msg["params"]["update"];
            match u.get("sessionUpdate").and_then(|v| v.as_str()) {
                Some("agent_message_chunk") => {
                    if let Some(t) = u["content"]["text"].as_str() {
                        outcome.final_message.push_str(t);
                    }
                }
                Some("tool_call") => {
                    if let Some(title) = u.get("title").and_then(|v| v.as_str()) {
                        outcome.tool_calls.push(title.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    let _ = child.kill().await;
    Ok(outcome)
}

async fn send_request(
    stdin: &mut ChildStdin,
    next_id: &mut i64,
    method: &str,
    params: Value,
) -> Result<()> {
    let id = *next_id;
    *next_id += 1;
    let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    stdin
        .write_all(format!("{}\n", msg).as_bytes())
        .await
        .map_err(|e| BitrouterError::internal(format!("write {method}: {e}")))?;
    Ok(())
}

async fn wait_for_result(lines: &mut Lines<BufReader<ChildStdout>>, id: i64) -> Result<Value> {
    loop {
        let line = lines
            .next_line()
            .await
            .map_err(|e| BitrouterError::internal(format!("read: {e}")))?
            .ok_or_else(|| BitrouterError::internal("agent closed stdout before responding"))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
            if let Some(err) = msg.get("error") {
                return Err(BitrouterError::internal(format!(
                    "agent error on id {id}: {err}"
                )));
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drives_fake_agent_to_end_turn() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake_acp_agent.mjs"
        );
        let mut env = BTreeMap::new();
        env.insert("NODE_NO_WARNINGS".to_string(), "1".to_string());
        let spawn = WorkerSpawn {
            command: "node".to_string(),
            args: vec![fixture.to_string()],
            env,
            working_dir: None,
        };
        let out = drive_once(spawn, "do the task at /tmp/out.txt")
            .await
            .unwrap();
        assert_eq!(out.stop_reason.as_deref(), Some("end_turn"));
        assert!(out.final_message.contains("done"));
        assert_eq!(out.tool_calls, vec!["write /tmp/out.txt".to_string()]);
    }
}
