//! Drive both manager-facing stdio and adapter-facing stdio through the installed BRO.
#[path = "support/binary.rs"]
mod binary;
#[path = "support/acp.rs"]
mod support;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

const TOKEN: &str = "brk_synthetic-acp-bound-token";
const DEADLINE: Duration = Duration::from_secs(10);

struct Manager {
    child: Child,
    writer: ChildStdin,
    reader: Lines<BufReader<ChildStdout>>,
    dir: tempfile::TempDir,
}

enum Adapter<'a> {
    Configured,
    Direct,
    Gateway(&'a str),
}

const GATEWAY: Adapter<'static> = Adapter::Gateway("http://127.0.0.1:1");

impl Manager {
    fn spawn(mode: Adapter<'_>) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let mut cmd = Command::new(binary::bro());
        cmd.args(["acp", "serve"])
            .env("BITROUTER_API_KEY", TOKEN)
            .env("BRO_FIXTURE_PARENT_SECRET", "ambient")
            .env("BRO_HOME", dir.path().join("bro"));
        let adapter = support::fixture()?;
        if let Adapter::Gateway(endpoint) = mode {
            std::fs::copy(
                adapter,
                dir.path()
                    .join(format!("npx{}", std::env::consts::EXE_SUFFIX)),
            )?;
            cmd.arg("codex")
                .env("PATH", dir.path())
                .env("BITROUTER_ENDPOINT", endpoint);
        } else {
            let config = dir.path().join("agents.yaml");
            std::fs::write(
                &config,
                serde_json::to_vec(&json!({"agents":{"stub":{"name":"stub","transport":{
                    "type":"stdio","command":adapter,"args":["--serve-compat"],"env":{"BRO_FIXTURE_LOCAL":"local"}
                }}}}))?,
            )?;
            if matches!(mode, Adapter::Direct) {
                cmd.arg("--direct");
            } else {
                cmd.env_remove("BITROUTER_ENDPOINT")
                    .env_remove("BITROUTER_API_KEY");
            }
            cmd.args(["--agent", "stub", "--config"]).arg(config);
        }
        let mut child = cmd
            .current_dir(dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let writer = child.stdin.take().context("missing manager stdin")?;
        let reader = BufReader::new(child.stdout.take().context("missing manager stdout")?).lines();
        Ok(Self {
            child,
            writer,
            reader,
            dir,
        })
    }
    async fn write(&mut self, frame: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&frame)?;
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await?;
        self.writer.flush().await?;
        Ok(())
    }
    async fn read(&mut self) -> Result<Value> {
        let line = timeout(DEADLINE, self.reader.next_line())
            .await??
            .context("BRO disconnected")?;
        ensure!(!line.contains(TOKEN), "gateway token on the manager wire");
        Ok(serde_json::from_str(&line)?)
    }
    async fn request(
        &mut self,
        id: Value,
        method: &str,
        params: Value,
    ) -> Result<(Value, Vec<Value>)> {
        self.write(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        timeout(DEADLINE, async {
            let mut notices = Vec::new();
            loop {
                let frame = self.read().await?;
                if frame["id"] == id && frame.get("method").is_none() {
                    return Ok((frame, notices));
                }
                notices.push(frame);
            }
        })
        .await?
    }
    async fn init(&mut self) -> Result<Value> {
        let (resp, _) = self
            .request(json!(1), "initialize", json!({"protocolVersion":1}))
            .await?;
        ensure!(
            resp["result"]["agentInfo"]["name"] == "bitrouter-acp-controller",
            "{resp}"
        );
        ensure!(resp["result"]["agentCapabilities"]["providers"].is_null());
        Ok(resp["result"].clone())
    }
    async fn new_session(&mut self) -> Result<Value> {
        let cwd = self.dir.path().canonicalize()?;
        let (resp, _) = self
            .request(
                json!("new"),
                "session/new",
                json!({"cwd":cwd,"mcpServers":[]}),
            )
            .await?;
        ensure!(
            resp["result"].get("record_id").is_none() && resp["result"]["sessionId"].is_string()
        );
        Ok(resp["result"]["sessionId"].clone())
    }
    async fn disconnect(self) -> Result<tempfile::TempDir> {
        let Self {
            mut child,
            writer,
            reader: _reader,
            dir,
        } = self;
        drop(writer);
        ensure!(
            timeout(Duration::from_secs(5), child.wait())
                .await??
                .success()
        );
        Ok(dir)
    }
}

fn prompt(session: &Value, text: &str) -> Value {
    json!({"sessionId":session,"prompt":[{"type":"text","text":text}]})
}

#[tokio::test]
async fn serve_subprocess_e2e() -> Result<()> {
    let mut manager = Manager::spawn(Adapter::Configured)?;
    let init = manager.init().await?;
    assert_eq!(
        init["_meta"]["bitrouter.dev/controller"]["upstreamAgentInfo"]["name"],
        "stub-harness"
    );
    assert_eq!(init["agentCapabilities"]["loadSession"], true);
    let a = manager.new_session().await?;
    let b = manager.new_session().await?;
    assert_eq!(a, "native-a");
    assert_eq!(b, "native-b");
    let (loaded, _) = manager
        .request(
            json!("load"),
            "session/load",
            json!({"sessionId":a,"cwd":manager.dir.path(),"mcpServers":[]}),
        )
        .await?;
    assert_eq!(loaded["result"]["_meta"]["loadedBy"], "harness");
    let (result, events) = manager
        .request(json!(5), "session/prompt", prompt(&b, "do X"))
        .await?;
    assert_eq!(result["result"]["stopReason"], "end_turn");
    assert!(events.iter().any(|n| n["method"] == "session/update"
        && n["params"]["sessionId"] == b
        && n["params"]["update"]["content"]["text"] == "hi"));
    manager.disconnect().await?;
    Ok(())
}

#[tokio::test]
async fn conformance_forwarded_update_variants_survive_round_trip() -> Result<()> {
    let mut manager = Manager::spawn(Adapter::Direct)?;
    manager.init().await?;
    let session = manager.new_session().await?;
    let (result, events) = manager
        .request(json!("p"), "session/prompt", prompt(&session, "go"))
        .await?;
    assert_eq!(result["result"]["stopReason"], "end_turn");
    for kind in [
        "plan",
        "available_commands_update",
        "current_mode_update",
        "config_option_update",
        "session_info_update",
    ] {
        ensure!(
            events.iter().any(|n| n["method"] == "session/update"
                && n["params"]["sessionId"] == session
                && n["params"]["update"]["sessionUpdate"] == kind),
            "missing {kind}"
        );
    }
    manager.disconnect().await?;
    Ok(())
}

#[tokio::test]
async fn serve_warns_about_ignored_plugin_blocks() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("agents.yaml");
    std::fs::write(&path, "plugins:\n  bitrouter-observe:\n    enabled: true\n")?;
    let output = timeout(
        DEADLINE,
        Command::new(binary::bro())
            .args([
                "acp",
                "serve",
                "--agent",
                "no-such-agent",
                "--direct",
                "--config",
            ])
            .arg(path)
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let error = std::str::from_utf8(&output.stderr)?;
    ensure!(error.contains("plugins.bitrouter-observe is not read by this binary and is ignored"));
    ensure!(
        !output.status.success()
            && error.contains("unknown ACP adapter")
            && output.stdout.is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn routed_serve_gates_provider_methods_and_forwards_manager_permissions() -> Result<()> {
    let mut manager = Manager::spawn(GATEWAY)?;
    let (early, _) = manager.request(json!(0), "session/new", json!({})).await?;
    assert_eq!(early["error"]["code"], -32002);
    manager.init().await?;
    for method in [
        "providers/set",
        "providers/list",
        "_bitrouter/route/set",
        "initialize",
    ] {
        let (blocked, _) = manager.request(json!(7), method, json!({})).await?;
        ensure!(blocked["error"]["code"].is_number(), "{method} accepted");
    }
    let session = manager.new_session().await?;
    manager.write(json!({"jsonrpc":"2.0","method":"session/prompt","id":"incoming","params":prompt(&session,"ask")})).await?;
    let callback = manager.read().await?;
    assert_eq!(callback["method"], "session/request_permission");
    assert_eq!(callback["params"]["sessionId"], session);
    manager.write(json!({"jsonrpc":"2.0","id":callback["id"],"result":{"outcome":{"outcome":"selected","optionId":"deny"}}})).await?;
    let response = manager.read().await?;
    assert_eq!(response["id"], "incoming");
    assert_eq!(response["result"]["stopReason"], "end_turn");
    let (resp, events) = manager
        .request(json!("again"), "session/prompt", prompt(&session, "hello"))
        .await?;
    assert_eq!(resp["result"]["stopReason"], "end_turn");
    ensure!(
        events
            .iter()
            .any(|e| e["params"]["update"]["content"]["text"] == "adapter reply [REDACTED]")
    );
    manager.disconnect().await?;
    Ok(())
}

async fn wait_file(path: &Path) -> Result<()> {
    timeout(DEADLINE, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn manager_eof_reaps_adapter_descendant_during_unanswered_prompt() -> Result<()> {
    let mut manager = Manager::spawn(GATEWAY)?;
    manager.init().await?;
    let session = manager.new_session().await?;
    manager.write(json!({"jsonrpc":"2.0","id":"p","method":"session/prompt","params":prompt(&session,"hang-with-child")})).await?;
    wait_file(&manager.dir.path().join("descendant-ready")).await?;
    let socket = std::fs::read_to_string(manager.dir.path().join("descendant-ready"))?;
    ensure!(
        std::net::TcpListener::bind(&socket).is_err(),
        "fixture did not hold its socket"
    );
    let dir = manager.disconnect().await?;
    let _released =
        std::net::TcpListener::bind(socket).context("adapter descendant still owns its socket")?;
    std::fs::write(dir.path().join("release"), "release")?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    ensure!(
        !dir.path().join("orphan-effect").exists(),
        "adapter descendant outlived BRO"
    );
    Ok(())
}

#[tokio::test]
async fn mismatched_gateway_fails_initialize_while_manager_keeps_stdin_open() -> Result<()> {
    let mut manager = Manager::spawn(Adapter::Gateway("http://127.0.0.1:1/mismatch"))?;
    let (response, notices) = manager
        .request(json!(1), "initialize", json!({"protocolVersion":1}))
        .await?;
    ensure!(notices.is_empty() && response["error"]["code"] == -32603);
    ensure!(!timeout(DEADLINE, manager.child.wait()).await??.success());
    Ok(())
}

#[tokio::test]
async fn adapter_disconnect_exits_with_manager_stdin_still_open() -> Result<()> {
    let mut manager = Manager::spawn(GATEWAY)?;
    manager.init().await?;
    let session = manager.new_session().await?;
    manager.write(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":prompt(&session,"disconnect")})).await?;
    ensure!(!timeout(DEADLINE, manager.child.wait()).await??.success());
    ensure!(manager.reader.next_line().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn manager_cancel_reaches_pending_prompt_and_leaves_session_usable() -> Result<()> {
    let mut manager = Manager::spawn(GATEWAY)?;
    manager.init().await?;
    let session = manager.new_session().await?;
    manager.write(json!({"jsonrpc":"2.0","id":8,"method":"session/prompt","params":prompt(&session,"hang")})).await?;
    wait_file(&manager.dir.path().join("prompt-received")).await?;
    manager
        .write(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}))
        .await?;
    let cancelled = manager.read().await?;
    assert_eq!(cancelled["id"], 8);
    assert_eq!(cancelled["result"]["stopReason"], "cancelled");
    let (reply, _) = manager
        .request(json!(9), "session/prompt", prompt(&session, "hello"))
        .await?;
    assert_eq!(reply["result"]["stopReason"], "end_turn");
    manager.disconnect().await?;
    Ok(())
}
