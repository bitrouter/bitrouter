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
        let mut correlation = None;
        loop {
            line.clear();
            ensure!(reader.read_line(&mut line)? > 0, "incomplete HTTP request");
            if line == "\r\n" {
                break;
            }
            if let Some((key, val)) = line.split_once(':')
                && key.eq_ignore_ascii_case("x-bitrouter-request-id")
            {
                correlation = Some(val.trim().to_owned());
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
        let correlation = correlation.context("missing caller correlation")?;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nX-BitRouter-Request-Id: {correlation}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
