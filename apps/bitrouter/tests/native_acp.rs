//! Product stdio bridge, daemon ownership and durable SQLite ACP history.
#![cfg(unix)]

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Peer {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    seen: Vec<Value>,
}
impl Peer {
    async fn connect(config: &Path, home: &Path, version: u32) -> Result<Self> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_bro"))
            .args([
                "acp",
                "serve",
                "--model",
                "test-model",
                "--no-start",
                "--config",
            ])
            .arg(config)
            .env("BITROUTER_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().context("stdin missing")?;
        let output = BufReader::new(child.stdout.take().context("stdout missing")?);
        let mut peer = Self {
            child,
            input,
            output,
            seen: Vec::new(),
        };
        let initialized=peer.request(1,"initialize",json!({"protocolVersion":version,"info":{"name":"process-test","version":"1"},"clientInfo":{"name":"process-test","version":"1"}})).await?;
        ensure!(
            initialized["protocolVersion"] == version,
            "version negotiation failed: {initialized}"
        );
        Ok(peer)
    }
    async fn send(&mut self, value: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        self.input.write_all(&bytes).await?;
        Ok(())
    }
    async fn receive(&mut self, matches: impl Fn(&Value) -> bool) -> Result<Value> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let mut line = String::new();
                ensure!(
                    self.output.read_line(&mut line).await? > 0,
                    "ACP bridge ended"
                );
                let value: Value =
                    serde_json::from_str(&line).context("stdout must contain only ACP JSON-RPC")?;
                self.seen.push(value.clone());
                if matches(&value) {
                    return Ok(value);
                }
            }
        })
        .await?
    }
    async fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        let reply = self
            .receive(|v| v["id"] == id && (v.get("result").is_some() || v.get("error").is_some()))
            .await?;
        ensure!(reply.get("error").is_none(), "{method}: {reply}");
        Ok(reply["result"].clone())
    }
    async fn detach(mut self) -> Result<()> {
        self.input.shutdown().await?;
        drop(self.input);
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await??;
        ensure!(status.success(), "bridge detach failed: {status}");
        Ok(())
    }
}

#[tokio::test]
async fn stdio_disconnect_restores_same_approval_across_versions_and_daemon_restart() -> Result<()>
{
    for version in [1, 2] {
        let upstream = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        Mock::given(method("POST")).and(path("/chat/completions")).respond_with(move |_:&wiremock::Request| {
            let delta=if count.fetch_add(1,Ordering::SeqCst)==0 {
                json!({"tool_calls":[{"index":0,"id":"fixture-write","type":"function","function":{"name":"write","arguments":"{\"path\":\"once.txt\",\"content\":\"approved\"}"}}]})
            } else {json!({"content":"Finished once."})};
            let finish=if delta.get("tool_calls").is_some(){"tool_calls"}else{"stop"};
            let body=format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",json!({"id":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]}));
            ResponseTemplate::new(200).set_body_raw(body,"text/event-stream")
        }).mount(&upstream).await;
        let home = tempfile::tempdir()?;
        let project = home.path().join("project");
        std::fs::create_dir(&project)?;
        let config = home.path().join("bitrouter.yaml");
        std::fs::write(
            &config,
            format!(
                "inherit_defaults: false\nserver:\n  listen: '127.0.0.1:0'\n  skip_auth: true\ndatabase:\n  url: 'sqlite://{}?mode=rwc'\nproviders:\n  mock:\n    api_base: {}\n    api_key: fixture\n    api_protocol:\n      - '*': chat_completions\n    models:\n      - id: test-model\n",
                home.path().join("bitrouter.db").display(),
                upstream.uri()
            ),
        )?;
        let launch = || -> Result<Child> {
            Ok(Command::new(env!("CARGO_BIN_EXE_bro"))
                .args(["serve", "--config"])
                .arg(&config)
                .env("BITROUTER_HOME", home.path())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()?)
        };
        let mut daemon = launch()?;
        let source = bitrouter::paths::ConfigSource::File(config.clone());
        let cfg = bitrouter::paths::load_config(&source).await?;
        let socket = bitrouter::daemon::socket_path_for(&source, &cfg);
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if bitrouter::daemon::probe_status(&socket)
                    .await
                    .ok()
                    .flatten()
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        let mut first = Peer::connect(&config, home.path(), version).await?;
        let setup = json!({"cwd":project,"mcpServers":[]});
        let session = first.request(2, "session/new", setup.clone()).await?["sessionId"]
            .as_str()
            .context("session missing")?
            .to_string();
        first.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"write once"}]}})).await?;
        let approval = first
            .receive(|v| v["method"] == "session/request_permission")
            .await?;
        let native_input = approval
            .pointer("/params/_meta/bitrouter/requestId")
            .context("input missing")?
            .clone();
        first.detach().await?;
        ensure!(
            !project.join("once.txt").exists(),
            "disconnect approved an effect"
        );
        let mut second = Peer::connect(&config, home.path(), 3 - version).await?;
        let mut reopen = setup.clone();
        reopen["sessionId"] = json!(session);
        if version == 1 {
            reopen["replayFrom"] = json!({"type":"start"});
        }
        second
            .request(
                2,
                if version == 1 {
                    "session/resume"
                } else {
                    "session/load"
                },
                reopen,
            )
            .await?;
        let permission = second
            .receive(|v| v["method"] == "session/request_permission")
            .await?;
        ensure!(
            permission.pointer("/params/_meta/bitrouter/requestId") == Some(&native_input),
            "approval identity changed"
        );
        second.send(json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}})).await?;
        second
            .receive(|v| {
                v.pointer("/params/update/content/text") == Some(&json!("Finished once."))
                    || v.pointer("/params/update/content/0/text") == Some(&json!("Finished once."))
            })
            .await?;
        let mut observer = Peer::connect(&config, home.path(), 2).await?;
        let mut reopen = setup.clone();
        reopen["sessionId"] = json!(session);
        reopen["replayFrom"] = json!({"type":"start"});
        observer
            .request(2, "session/resume", reopen.clone())
            .await?;
        if !observer.seen.iter().any(|v| {
            v.pointer("/params/update/state") == Some(&json!("idle"))
                && v.pointer("/params/update/stopReason") == Some(&json!("end_turn"))
        }) {
            observer
                .receive(|v| {
                    v.pointer("/params/update/state") == Some(&json!("idle"))
                        && v.pointer("/params/update/stopReason") == Some(&json!("end_turn"))
                })
                .await?;
        }
        ensure!(std::fs::read_to_string(project.join("once.txt"))? == "approved");
        ensure!(calls.load(Ordering::SeqCst) == 2);
        observer
            .request(3, "session/close", json!({"sessionId":session}))
            .await?;
        observer.detach().await?;
        second.detach().await?;
        let stopped = Command::new(env!("CARGO_BIN_EXE_bro"))
            .args(["stop", "--config"])
            .arg(&config)
            .env("BITROUTER_HOME", home.path())
            .output()
            .await?;
        ensure!(stopped.status.success(), "stop failed");
        tokio::time::timeout(Duration::from_secs(10), daemon.wait()).await??;
        daemon = launch()?;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if bitrouter::daemon::probe_status(&socket)
                    .await
                    .ok()
                    .flatten()
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        let mut cold = Peer::connect(&config, home.path(), 2).await?;
        cold.request(2, "session/resume", reopen).await?;
        ensure!(
            cold.seen
                .iter()
                .any(|v| v.pointer("/params/update/content/0/text")
                    == Some(&json!("Finished once."))),
            "cold history missing"
        );
        ensure!(
            calls.load(Ordering::SeqCst) == 2,
            "load replayed model work"
        );
        cold.detach().await?;
        let _ = Command::new(env!("CARGO_BIN_EXE_bro"))
            .args(["stop", "--config"])
            .arg(&config)
            .env("BITROUTER_HOME", home.path())
            .output()
            .await?;
        tokio::time::timeout(Duration::from_secs(10), daemon.wait()).await??;
    }
    Ok(())
}
