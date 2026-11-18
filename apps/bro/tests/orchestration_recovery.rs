//! Every operation below starts a fresh process. The mock sees only new turns.
#[path = "support/binary.rs"]
mod binary;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

fn bro(home: &Path, args: &[&str]) -> Result<Output> {
    Ok(Command::new(binary::bro())
        .arg("--data-dir")
        .arg(home)
        .args(args)
        .env_remove("BITROUTER_ENDPOINT")
        .env_remove("BITROUTER_API_KEY")
        .output()?)
}
fn last(home: &Path, args: &[&str]) -> Result<Value> {
    let out = bro(home, args)?;
    ensure!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(serde_json::from_slice(
        out.stdout
            .split(|b| *b == b'\n')
            .rfind(|l| !l.is_empty())
            .context("missing CLI result")?,
    )?)
}

fn gateway() -> Result<(String, thread::JoinHandle<Result<Value>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    listener.set_nonblocking(true)?;
    let mock = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut stream = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(e) => return Err(e.into()),
            }
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        let mut len = None;
        loop {
            line.clear();
            ensure!(reader.read_line(&mut line)? > 0, "incomplete HTTP request");
            if line == "\r\n" {
                break;
            }
            if let Some((key, val)) = line.split_once(':')
                && key.eq_ignore_ascii_case("content-length")
            {
                len = Some(val.trim().parse()?);
            }
        }
        let mut bytes = vec![0; len.context("missing length")?];
        reader.read_exact(&mut bytes)?;
        let body = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"object":"chat.completion.chunk","id":"new","model":"fixture","choices":[{"index":0,"delta":{"role":"assistant","content":"new answer"},"finish_reason":"stop"}]})
        );
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        Ok(serde_json::from_slice(&bytes)?)
    });
    Ok((endpoint, mock))
}

#[test]
fn child_delegation_wait_wake_and_cancel_survive_restarts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("bro");
    let workspace = dir.path().join("workspace");
    fs::create_dir(&workspace)?;
    let (endpoint, mock) = gateway()?;
    let parent = last(
        &home,
        &[
            "--endpoint",
            &endpoint,
            "thread",
            "create",
            "--read-only",
            "--workspace",
            workspace.to_str().context("workspace")?,
        ],
    )?;
    let parent = parent["thread_id"].as_str().context("missing parent")?;
    let child = uuid::Uuid::new_v4().to_string();
    let create = ["agent", "create", parent, &child];
    last(&home, &create)?;
    last(&home, &create)?;
    let delegate = [
        "agent",
        "delegate",
        parent,
        &child,
        "--request-id",
        "one",
        "do work",
    ];
    let queued = last(&home, &delegate)?;
    let turn = queued["turn_id"].as_str().context("missing turn")?;
    ensure!(last(&home, &delegate)?["turn_id"] == turn);
    ensure!(
        !bro(
            &home,
            &[
                "agent",
                "delegate",
                parent,
                &child,
                "--request-id",
                "one",
                "changed"
            ]
        )?
        .status
        .success()
    );
    ensure!(
        last(
            &home,
            &["agent", "wait", parent, &child, turn, "--timeout-ms", "0"]
        )?["timed_out"]
            == true
    );
    let wake = ["agent", "wake", parent, &child, turn];
    ensure!(last(&home, &wake)?["status"] == "completed");
    let req = mock.join().map_err(|_| anyhow::anyhow!("lost mock"))??;
    ensure!(
        req["messages"]
            .as_array()
            .and_then(|m| m.last())
            .is_some_and(|m| m["role"] == "user" && m["content"] == "do work")
    );
    ensure!(last(&home, &wake)?["status"] == "completed"); // listener is gone
    ensure!(
        last(&home, &["agent", "wait", parent, &child, turn])?["outcome"]["final_answer"]
            == "new answer"
    );
    let two = last(
        &home,
        &[
            "agent",
            "delegate",
            parent,
            &child,
            "--request-id",
            "two",
            "cancel before dispatch",
        ],
    )?;
    let next = two["turn_id"].as_str().context("missing second turn")?;
    ensure!(last(&home, &["agent", "cancel", parent, &child, next])?["status"] == "cancelled");
    ensure!(last(&home, &["agent", "wait", parent, &child, next])?["status"] == "cancelled");
    let cancelled = bro(&home, &["agent", "wake", parent, &child, next])?;
    ensure!(
        !cancelled.status.success()
            && serde_json::from_slice::<Value>(&cancelled.stdout)?["status"] == "cancelled"
    );
    ensure!(last(&home, &["agent", "cancel", parent, &child, next])?["status"] == "cancelled");
    ensure!(
        !bro(&home, &["agent", "wake", &child, &child, next])?
            .status
            .success()
    );
    Ok(())
}

#[test]
fn transferred_prefix_and_call_pair_are_checked_before_binding() -> Result<()> {
    use sha2::{Digest, Sha256};
    use sqlx::{ConnectOptions, sqlite::SqliteConnectOptions};
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("bro");
    fs::create_dir_all(home.join("threads"))?;
    let workspace = dir.path().join("workspace");
    fs::create_dir(&workspace)?;
    let store = home.join("store.sqlite");
    let id = uuid::Uuid::new_v4().to_string();
    let old = uuid::Uuid::new_v4().to_string();
    let messages = json!([{"role":"user","content":"read evidence"},{"role":"assistant","tool_calls":[{"id":"read-1","type":"function","function":{"name":"read","arguments":"{}"}}]},{"role":"tool","tool_call_id":"read-1","content":"sealed result"},{"role":"assistant","content":"old answer"}]);
    let origin = json!({"format":"bro-local/1","thread_id":id,"workspace":"/old","endpoint":"","model":"fixture","effort":null,"max_output_tokens":null,"tool_mode":"read_only","turns":[{"turn_id":old,"prompt":"read evidence","request_id":"old-key","steps":[],"sealed_messages":messages,"outcome":{"status":"completed","detail":"completed","final_answer":"old answer"}}]});
    let origins = serde_json::to_vec(&json!({&id:&origin}))?;
    tokio::runtime::Runtime::new()?.block_on(async {
        let mut db = SqliteConnectOptions::new()
            .create_if_missing(true)
            .filename(&store)
            .connect()
            .await?;
        for table in [
            "acp_assessment_revisions",
            "acp_canonical_sessions",
            "acp_capture_connections",
            "acp_capture_events",
            "acp_checkpoint_members",
            "acp_checkpoints",
            "acp_effective_assessments",
            "acp_resource_observations",
            "adequacy_exploration",
            "adequacy_pins",
            "adequacy_reliability_events",
            "adequacy_semantic_success",
            "bro_acceptance_keys",
            "bro_execution_index",
            "bro_execution_records",
            "bro_executions",
            "bro_managed_sessions",
            "bro_runtime_owners",
            "bro_runtime_ownership",
            "checkpoint_evolution",
        ] {
            sqlx::query(&format!("CREATE TABLE {table}(id TEXT)"))
                .execute(&mut db)
                .await?;
        }
        sqlx::query("CREATE TABLE seaql_migrations(version TEXT)")
            .execute(&mut db)
            .await?;
        sqlx::query("INSERT INTO seaql_migrations VALUES(?)")
            .bind(format!(
                "m20261118_000004_bro_sqlite:sealed-v1:{}",
                hex::encode(Sha256::digest(&origins))
            ))
            .execute(&mut db)
            .await?;
        Ok::<(), anyhow::Error>(())
    })?;
    fs::write(home.join("sealed.json"), &origins)?;
    fs::write(
        home.join("ACTIVATED"),
        format!(
            "m20261118_000004_bro_sqlite\nsealed-v1 {} {}\n",
            hex::encode(Sha256::digest(fs::read(&store)?)),
            hex::encode(Sha256::digest(&origins))
        ),
    )?;
    let path = home.join("threads").join(format!("{id}.json"));
    let (endpoint, mock) = gateway()?;
    let restore = [
        "--endpoint",
        &endpoint,
        "managed",
        "restore",
        &id,
        "--workspace",
        workspace.to_str().context("workspace")?,
    ];
    let mut escalated = origin.clone();
    escalated["format"] = json!("bro-local/2");
    fs::write(&path, serde_json::to_vec(&escalated)?)?;
    let denied = bro(&home, &restore)?;
    ensure!(
        !denied.status.success()
            && String::from_utf8_lossy(&denied.stderr).contains("sealed M0 history")
    );
    let mut stale = origin.clone();
    stale["turns"][0]["sealed_messages"][2]["content"] = json!("stale result");
    fs::write(&path, serde_json::to_vec(&stale)?)?;
    let denied = bro(&home, &restore)?;
    ensure!(
        !denied.status.success()
            && String::from_utf8_lossy(&denied.stderr).contains("sealed M0 history")
    );
    // Both JSON files and the external marker agree with one another. The
    // SQLite ledger still commits to the originally authorized prefix.
    let marker = fs::read(home.join("ACTIVATED"))?;
    let forged = serde_json::to_vec(&json!({&id:stale}))?;
    fs::write(home.join("sealed.json"), &forged)?;
    fs::write(
        home.join("ACTIVATED"),
        format!(
            "m20261118_000004_bro_sqlite\nsealed-v1 {} {}\n",
            hex::encode(Sha256::digest(fs::read(&store)?)),
            hex::encode(Sha256::digest(forged))
        ),
    )?;
    let denied = bro(&home, &restore)?;
    ensure!(
        !denied.status.success()
            && String::from_utf8_lossy(&denied.stderr).contains("digest mismatch")
    );
    fs::write(home.join("ACTIVATED"), marker)?;
    fs::write(home.join("sealed.json"), origins)?;
    fs::write(&path, serde_json::to_vec(&origin)?)?;
    last(&home, &restore)?;
    ensure!(
        last(
            &home,
            &[
                "thread",
                "start",
                &id,
                "--request-id",
                "old-key",
                "read evidence"
            ]
        )?["turn_id"]
            == old
    );
    let new = last(
        &home,
        &[
            "thread",
            "start",
            &id,
            "--request-id",
            "new-key",
            "continue",
        ],
    )?;
    ensure!(new["status"] == "completed");
    let req = mock.join().map_err(|_| anyhow::anyhow!("lost mock"))??;
    let context = req["messages"].as_array().context("missing SDK context")?;
    ensure!(
        context
            .iter()
            .filter(|m| m["tool_call_id"] == "read-1" && m["content"] == "sealed result")
            .count()
            == 1
    );
    ensure!(
        context
            .last()
            .is_some_and(|m| m["role"] == "user" && m["content"] == "continue")
    );
    last(&home, &restore)?;
    ensure!(
        last(
            &home,
            &[
                "thread",
                "start",
                &id,
                "--request-id",
                "new-key",
                "continue"
            ]
        )?["turn_id"]
            == new["turn_id"]
    );
    Ok(())
}
