//! Replay the v7 writer's actual transcript using cold BRO processes.
use super::{REQUESTS, THREAD, capture, captured, mixed_store, settle};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

async fn replay(
    bro: &Path,
    home: &Path,
    work: &Path,
    endpoint: &str,
    thread: &str,
    version: u64,
) -> Result<Vec<Value>> {
    let mut peer = Command::new(bro)
        .arg("--data-dir")
        .arg(home)
        .args(["--endpoint", endpoint, "acp", "serve"])
        .env_remove("BITROUTER_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = peer.stdin.take().context("ACP stdin")?;
    let mut params = json!({"cwd":work,"sessionId":thread,"mcpServers":[]});
    let method = if version == 1 {
        "session/load"
    } else {
        params["replayFrom"] = json!({"type":"start"});
        "session/resume"
    };
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":version,"info":{"name":"cold replay","version":"1"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":method,"params":params}),
    ] {
        input.write_all(format!("{request}\n").as_bytes()).await?;
    }
    drop(input);
    let result = timeout(Duration::from_secs(15), peer.wait_with_output()).await??;
    ensure!(
        result.status.success(),
        "BRO ACP: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let frames = serde_json::Deserializer::from_slice(&result.stdout)
        .into_iter::<Value>()
        .collect::<serde_json::Result<Vec<_>>>()?;
    ensure!(
        frames
            .first()
            .is_some_and(|f| f["id"] == 1 && f["result"]["protocolVersion"] == version)
    );
    ensure!(
        frames.last().is_some_and(|f| f["id"] == 2
            && f["result"]["_meta"]["bitrouter"]["latestTurn"]["status"] == "completed"),
        "reopen: {:?}",
        frames.last()
    );
    let mut updates = Vec::new();
    for f in &frames[1..frames.len() - 1] {
        ensure!(f["method"] == "session/update" && f["params"]["sessionId"] == thread);
        updates.push(f["params"]["update"].clone());
    }
    Ok(updates)
}

#[tokio::test]
#[ignore = "requires the separately built BRO in BITROUTER_TEST_BRO"]
async fn cold_restore_replays_the_captured_transcript_once() -> Result<()> {
    for tool in ["read", "write"] {
        verify_capture(tool).await?;
    }
    Ok(())
}

async fn verify_capture(tool: &str) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let work = tempfile::tempdir()?;
    let (source, thread, prompt, call, input, output, answer) = if tool == "write" {
        let (source, db) = super::super::fresh(dir.path()).await?;
        let thread = capture::install_approved(dir.path(), &db).await?;
        db.close().await?;
        (
            source,
            thread,
            "Write the approved content to evidence.txt.",
            "write-evidence",
            json!({"path":"evidence.txt","content":"approved M0 write\n"}),
            json!({"type":"json","value":{"bytes":18,"path":"evidence.txt"}}),
            "Wrote evidence.txt.",
        )
    } else {
        let (source, db) = captured(dir.path()).await?;
        for id in REQUESTS {
            settle(&db, id, 0).await?;
        }
        db.close().await?;
        (
            source,
            THREAD.to_owned(),
            "Read evidence.txt and report its contents.",
            "read-evidence",
            json!({"path":"evidence.txt","limit":10}),
            json!("File \"evidence.txt\"\nL1: baseline persisted payload\n"),
            "The evidence says baseline persisted payload.",
        )
    };
    let dest = dir.path().join("split");
    let result = mixed_store::activate(&source, &dest).await?;
    ensure!(result.dispatch_allowed, "{:?}", result.blockers);
    let home = dest.join("bro");
    let bro =
        std::path::PathBuf::from(std::env::var_os("BITROUTER_TEST_BRO").context("BRO executable")?);
    let gateway = wiremock::MockServer::start().await;
    let endpoint = gateway.uri();
    let restored = Command::new(&bro)
        .arg("--data-dir")
        .arg(&home)
        .args([
            "--endpoint",
            &endpoint,
            "managed",
            "restore",
            &thread,
            "--workspace",
        ])
        .arg(work.path())
        .output()
        .await?;
    ensure!(
        restored.status.success(),
        "BRO restore: {}",
        String::from_utf8_lossy(&restored.stderr)
    );
    let mut ids = Vec::new();
    for version in [1, 2] {
        let updates = replay(&bro, &home, work.path(), &endpoint, &thread, version).await?;
        let tags: Vec<_> = updates
            .iter()
            .map(|u| u["sessionUpdate"].as_str())
            .collect::<Option<_>>()
            .context("missing update kind")?;
        if version == 1 {
            ensure!(
                tags == [
                    "user_message_chunk",
                    "tool_call",
                    "tool_call_update",
                    "agent_message_chunk"
                ],
                "v1 transcript: {tags:?}"
            );
        } else {
            ensure!(
                tags == [
                    "user_message",
                    "tool_call_update",
                    "tool_call_update",
                    "agent_message",
                    "state_update"
                ],
                "v2 transcript: {tags:?}"
            );
            ensure!(updates[4]["state"] == "idle" && updates[4]["stopReason"] == "end_turn");
        }
        let content = |u: &Value| {
            if version == 1 {
                u["content"]["text"].clone()
            } else {
                u["content"][0]["text"].clone()
            }
        };
        ensure!(content(&updates[0]) == prompt);
        ensure!(
            updates[1]["status"] == "pending"
                && updates[1]["title"] == tool
                && updates[1]["toolCallId"] == call
        );
        ensure!(updates[1]["rawInput"] == input);
        ensure!(
            updates[2]["toolCallId"] == updates[1]["toolCallId"]
                && updates[2]["status"] == "completed"
        );
        ensure!(
            updates[2]["rawOutput"] == output,
            "unexpected sealed {tool} output: {}",
            updates[2]
        );
        ensure!(content(&updates[3]) == answer);
        let pair = [
            updates[0]["messageId"].clone(),
            updates[3]["messageId"].clone(),
        ];
        ensure!(pair.iter().all(Value::is_string) && pair[0] != pair[1]);
        ids.push(pair);
    }
    ensure!(ids[0] == ids[1]);
    ensure!(
        !work.path().join("evidence.txt").exists(),
        "cold replay executed a tool"
    );
    ensure!(
        gateway
            .received_requests()
            .await
            .context("gateway request log")?
            .is_empty()
    );
    Ok(())
}
