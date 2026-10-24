//! Owner-local CLI snapshots with real daemon and Cargo-built ACP fixtures.
#![cfg(unix)]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::process::{Child, Command};

#[path = "../../../crates/bitrouter-orchestrator/tests/support/example_binary.rs"]
mod example_binary;

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("BITROUTER_HOME", home)
        .env("NO_COLOR", "1")
        .current_dir(home)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

async fn invoke(home: &Path, args: &[&str]) -> Result<Value> {
    let output =
        tokio::time::timeout(Duration::from_secs(20), command(home).args(args).output()).await??;
    ensure!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        !output.stdout.contains(&0x1b),
        "CLI emitted terminal controls"
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

async fn launch(home: &Path) -> Result<Child> {
    let child = command(home)
        .args(["serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if bitrouter::daemon::probe_status(&home.join("bitrouter.sock"))
                .await?
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(child)
}

async fn idle(home: &Path) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let report = invoke(home, &["agents", "sessions"]).await?;
            if report["runs"]
                .as_array()
                .context("runs missing")?
                .iter()
                .all(|run| run["turn"] == "idle")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn connected_cli_separates_instances_catalog_history_and_stopped_daemon() -> Result<()> {
    let home = tempfile::tempdir_in("/tmp")?;
    let root = home.path();
    let fresh = invoke(root, &["agents"]).await?;
    assert_eq!(fresh["daemon_state"], "stopped");
    assert!(fresh["instances"].is_null());
    assert!(!root.join("bitrouter.yaml").exists());
    assert!(!root.join("bitrouter.pid").exists());
    let fixture =
        example_binary::build("bitrouter", "pty_acp_fixture").map_err(anyhow::Error::msg)?;
    std::fs::write(
        root.join("bitrouter.yaml"),
        format!(
            "inherit_defaults: false\nregistry: {{enabled: false}}\nserver: {{listen: '127.0.0.1:0', skip_auth: true}}\ndatabase: {{url: 'sqlite::memory:'}}\nagents:\n  stub:\n    name: stub\n    transport:\n      type: stdio\n      command: {}\n      env:\n        BITROUTER_PTY_SCENARIO: minimal\n        BITROUTER_PTY_CAPTURE: {}\n  broken:\n    name: broken\n    transport: {{type: stdio, command: '/no-such-connected-fixture'}}\n",
            serde_json::to_string(&fixture)?,
            serde_json::to_string(&root.join("capture.ndjson"))?
        ),
    )?;
    let stopped = invoke(root, &["agents"]).await?;
    assert_eq!(stopped["daemon_state"], "stopped");
    assert!(stopped["instances"].is_null());
    assert!(!root.join("capture.ndjson").exists());
    assert!(!root.join("bitrouter.pid").exists());
    let mut daemon = launch(root).await?;
    let empty = invoke(root, &["agents", "--json"]).await?;
    assert_eq!(empty["daemon_state"], "running");
    assert_eq!(empty["instances"], json!([]));
    let catalog = invoke(root, &["agents", "list"]).await?;
    ensure!(
        catalog["agents"]
            .as_array()
            .context("catalog missing")?
            .iter()
            .any(|row| row["id"] == "stub" && row["configured"] == true)
    );
    let mut runs = Vec::new();
    for index in 0..2 {
        let workspace = root.join(format!("project-{index}"));
        std::fs::create_dir(&workspace)?;
        let accepted = invoke(
            root,
            &[
                "run",
                "stub",
                "fixture prompt must not leak",
                "--direct",
                "--background",
                "--cwd",
                workspace.to_str().context("path")?,
            ],
        )
        .await?;
        runs.push(
            accepted["agent_run_id"]
                .as_str()
                .context("run missing")?
                .to_owned(),
        );
    }
    idle(root).await?;
    let connected = invoke(root, &["agents"]).await?;
    let rows = connected["instances"]
        .as_array()
        .context("instances missing")?;
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0]["id"], rows[1]["id"]);
    assert_eq!(rows[0]["session_id"], rows[1]["session_id"]); // harness-native IDs may collide
    for row in rows {
        assert_eq!(row["kind"], "supervised_acp");
        assert_eq!(row["state"], "idle");
        assert!(row["model"].is_null());
    }
    ensure!(!serde_json::to_string(&connected)?.contains("fixture prompt must not leak"));
    let page = invoke(root, &["agents", "--limit", "1"]).await?;
    let cursor = page["next_cursor"].as_str().context("cursor missing")?;
    let next = invoke(root, &["agents", "--limit", "1", "--after", cursor]).await?;
    assert_eq!(
        next["instances"].as_array().context("page missing")?.len(),
        1
    );
    assert_ne!(page["instances"][0]["id"], next["instances"][0]["id"]);
    assert!(next["next_cursor"].is_null());
    for args in [
        ["agents", "--limit", "17"],
        ["agents", "--after", "old-boot/supervised_acp:run"],
    ] {
        let output = command(root).args(args).output().await?;
        ensure!(!output.status.success(), "invalid page accepted");
    }
    let human = command(root).args(["agents", "--human"]).output().await?;
    ensure!(human.status.success() && !human.stdout.contains(&0x1b));
    ensure!(String::from_utf8(human.stdout)?.contains("supervised_acp"));
    for run in &runs {
        invoke(root, &["agents", "stop", run]).await?;
    }
    let failed = command(root)
        .args(["run", "broken", "failed", "--direct", "--background"])
        .output()
        .await?;
    ensure!(
        !failed.status.success(),
        "missing executable unexpectedly launched"
    );
    let history = invoke(root, &["agents", "sessions"]).await?;
    let history = history["runs"].as_array().context("history missing")?;
    ensure!(history.iter().any(|row| row["process"] == "stopped"));
    ensure!(history.iter().any(|row| row["process"] == "failed"));
    assert_eq!(invoke(root, &["agents"]).await?["instances"], json!([]));
    // A client-side config error does not hide a source-verified running daemon.
    std::fs::write(root.join("bitrouter.yaml"), "invalid: [")?;
    assert_eq!(invoke(root, &["agents"]).await?["daemon_state"], "running");
    bitrouter::daemon::send_command(
        &root.join("bitrouter.sock"),
        &bitrouter::daemon::DaemonCommand::Stop,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(10), daemon.wait()).await??;
    let explicit = invoke(
        root,
        &[
            "agents",
            "--socket",
            root.join("bitrouter.sock").to_str().context("socket")?,
        ],
    )
    .await?;
    assert_eq!(explicit["daemon_state"], "stopped");
    assert!(explicit["instances"].is_null());
    Ok(())
}

#[tokio::test]
async fn query_failures_are_errors_and_never_an_empty_connected_list() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    for response in [
        "not-json\n",
        "{\"resp\":\"error\",\"message\":\"snapshot unsupported\"}\n",
    ] {
        let home = tempfile::tempdir_in("/tmp")?;
        let socket = home.path().join("fixture.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            reader.get_mut().write_all(response.as_bytes()).await?;
            Ok::<_, std::io::Error>(())
        });
        let output = command(home.path())
            .args(["agents", "--socket"])
            .arg(&socket)
            .output()
            .await?;
        ensure!(!output.status.success());
        ensure!(!String::from_utf8_lossy(&output.stdout).contains("\"instances\": []"));
        server.await??;
    }
    Ok(())
}
