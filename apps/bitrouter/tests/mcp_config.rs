//! Actual CLI configuration transactions. All sources and processes are fixture-owned.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, ensure};
use bitrouter::config::mcp::{McpServerConfig, McpTransport};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[path = "harness_resources/mcp_fixture.rs"]
mod mcp_fixture;

fn cli(home: &Path, path: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("BITROUTER_HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .args(["mcp", args[0]])
        .arg("--config")
        .arg(path)
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn result(mut command: Command, input: Option<&str>, success: bool) -> Result<Value> {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn()?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .context("stdin")?
            .write_all(input.as_bytes())
            .await?;
    }
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
        .await??;
    ensure!(
        result.status.success() == success,
        "unexpected CLI result: {} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    ensure!(!result.stdout.contains(&0x1b), "ANSI in JSON output");
    serde_json::from_slice(&result.stdout).context("CLI JSON")
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_blocking_input_keeps_sigint_cancellation_and_source_bytes() -> Result<()> {
    use rustix::fs::{Mode, OFlags};
    let home = tempfile::tempdir()?;
    let path = home.path().join("config.yaml");
    let original = "inherit_defaults: false\n";
    std::fs::write(&path, original)?;
    let input = home.path().join("entry.fifo");
    // Opening the writer proves the CLI has entered its explicit input read.
    // https://pubs.opengroup.org/onlinepubs/9799919799/functions/open.html
    ensure!(
        Command::new("mkfifo").arg(&input).status().await?.success(),
        "could not create input FIFO"
    );
    let child = cli(home.path(), &path, &["add", "blocked", "--from"])
        .arg(&input)
        .spawn()?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let writer = loop {
        match rustix::fs::open(&input, OFlags::WRONLY | OFlags::NONBLOCK, Mode::empty()) {
            Ok(writer) => break writer,
            Err(rustix::io::Errno::NXIO) => {
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "CLI did not open explicit input"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id().context("child PID")?)?)
        .context("invalid child PID")?;
    rustix::process::kill_process(pid, rustix::process::Signal::INT)?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait_with_output())
        .await
        .context("SIGINT did not cancel explicit input")??;
    drop(writer);
    ensure!(!output.status.success());
    ensure!(std::fs::read_to_string(path)? == original);
    Ok(())
}

#[tokio::test]
async fn full_entries_crud_redaction_defaults_and_source_preservation() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("config.yaml");
    let initial = "# first\ninherit_defaults: false\nproviders: {} # do not rewrite\nchat:\n  model: 'unchanged' # keep ordering\n# end\n";
    std::fs::write(&path, initial)?;
    let http = json!({"name":"http","transport":{"type":"http","url":"https://user:password@example.com/mcp?token=query-secret#fragment","headers":{"Authorization":"Bearer ${MCP_SECRET}","X-Private":"header-secret"}},"aggregate":false,"tool_prefix":"custom__"});
    let input = serde_json::to_string(&http)?;
    let added = result(
        cli(home.path(), &path, &["add", "http", "--from", "-"]),
        Some(&input),
        true,
    )
    .await?;
    ensure!(
        added["activation"] == "saved_only"
            && added["running"] == "unknown"
            && added["restart_required"] == true
    );
    let saved = std::fs::read_to_string(&path)?;
    ensure!(saved.starts_with(initial));
    ensure!(saved.contains("${MCP_SECRET}") && !saved.contains("expanded-secret"));
    let listed = result(cli(home.path(), &path, &["list"]), None, true).await?;
    ensure!(listed["source"] == path.to_string_lossy().as_ref());
    ensure!(listed["servers"][0]["tool_prefix"] == "custom__");
    let mut shown = cli(home.path(), &path, &["show", "http"]);
    shown.env("MCP_SECRET", "expanded-secret");
    let text = serde_json::to_string(&result(shown, None, true).await?)?;
    for secret in [
        "password",
        "query-secret",
        "header-secret",
        "MCP_SECRET",
        "expanded-secret",
    ] {
        ensure!(!text.contains(secret), "secret disclosed");
    }
    let stdio = json!({"name":"stdio","transport":{"type":"stdio","command":"/must/not/be/executed","args":["arg-secret","--flag"],"env":{"TOKEN":"env-secret"}},"aggregate":false});
    let input_path = home.path().join("entry.json");
    std::fs::write(&input_path, serde_json::to_vec(&stdio)?)?;
    result(
        cli(
            home.path(),
            &path,
            &[
                "add",
                "stdio",
                "--from",
                input_path.to_str().context("input path")?,
            ],
        ),
        None,
        true,
    )
    .await?;
    let shown = result(cli(home.path(), &path, &["show", "stdio"]), None, true).await?;
    ensure!(shown["servers"][0]["aggregate"] == false);
    ensure!(
        !serde_json::to_string(&shown)?.contains("env-secret")
            && !serde_json::to_string(&shown)?.contains("arg-secret")
    );
    let edited = "name: http\ntransport:\n  type: stdio\n  command: /also/not/executed\n";
    result(
        cli(home.path(), &path, &["edit", "http", "--from", "-"]),
        Some(edited),
        true,
    )
    .await?;
    let shown = result(cli(home.path(), &path, &["show", "http"]), None, true).await?;
    ensure!(
        shown["servers"][0]["aggregate"] == true && shown["servers"][0]["tool_prefix"].is_null()
    );
    ensure!(shown["servers"][0]["transport"]["env"] == json!({}));
    result(cli(home.path(), &path, &["remove", "stdio"]), None, true).await?;
    result(cli(home.path(), &path, &["remove", "http"]), None, true).await?;
    let empty = result(cli(home.path(), &path, &["list"]), None, true).await?;
    ensure!(empty["servers"] == json!([]));
    ensure!(std::fs::read_to_string(&path)?.starts_with(initial));
    ensure!(
        !home.path().join("data").exists(),
        "management should not create execution storage"
    );
    // Runtime compatibility permits aliases in old files. Inspection must retain
    // the actual source key, so operators can explicitly repair the entry name.
    std::fs::write(
        &path,
        "inherit_defaults: false\nmcp_servers:\n  actual-key:\n    name: legacy-name\n    transport: {type: stdio, command: untouched}\n",
    )?;
    let old = result(cli(home.path(), &path, &["show", "actual-key"]), None, true).await?;
    ensure!(old["servers"][0]["id"] == "actual-key" && old["servers"][0]["name"] == "legacy-name");
    let listed = result(cli(home.path(), &path, &["list"]), None, true).await?;
    ensure!(listed["servers"][0]["id"] == "actual-key");
    result(
        cli(home.path(), &path, &["edit", "actual-key", "--from", "-"]),
        Some("name: actual-key\ntransport: {type: stdio, command: untouched}\n"),
        true,
    )
    .await?;
    let fixed: Value = serde_saphyr::from_str(&std::fs::read_to_string(&path)?)?;
    ensure!(
        fixed["mcp_servers"]["actual-key"]["name"] == "actual-key"
            && fixed["mcp_servers"]
                .as_object()
                .is_some_and(|m| m.len() == 1)
    );
    Ok(())
}

#[tokio::test]
async fn failures_are_noninteractive_secret_safe_and_do_not_replace_source() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("config.yaml");
    let original = "inherit_defaults: false\nmcp_servers:\n  one:\n    name: one\n    transport: {type: stdio, command: untouched}\n";
    std::fs::write(&path, original)?;
    for (args, input) in [
        (vec!["add", "one", "--url", "https://example.com"], None),
        (vec!["add", "missing"], None),
        (vec!["edit", "one"], None),
        (vec!["remove", "missing"], None),
        (vec!["show", "missing"], None),
        (
            vec!["edit", "one", "--from", "-"],
            Some("name: renamed\ntransport: {type: stdio, command: secret-command}\n"),
        ),
        (
            vec!["add", "new", "--from", "-"],
            Some("name: new\ntransport: {type: http, url: secret-invalid-url}\n"),
        ),
        (
            vec!["edit", "one", "--from", "-"],
            Some("name: one\ntransport: [secret-malformed-value\n"),
        ),
        (vec!["add", "sse", "--url", "https://example.com"], None),
    ] {
        let error = result(cli(home.path(), &path, &args), input, false).await?;
        ensure!(
            !serde_json::to_string(&error)?.contains("secret-"),
            "parser/validation disclosed a value"
        );
        ensure!(
            std::fs::read_to_string(&path)? == original,
            "failed command modified source"
        );
    }
    let mut human = cli(home.path(), &path, &["show", "one"]);
    human.arg("--human");
    let output = human.output().await?;
    ensure!(output.status.success() && String::from_utf8_lossy(&output.stdout).contains("Source:"));
    Ok(())
}

#[tokio::test]
async fn shortcut_creation_path_and_explicit_connect_only_on_check() -> Result<()> {
    let upstream = mcp_fixture::server(false).await;
    let home = tempfile::tempdir()?;
    let path = home.path().join("new.yaml");
    let configured = mcp_fixture::configuration(&upstream);
    let bitrouter::config::mcp::McpTransport::Http { url, .. } = configured.transport else {
        anyhow::bail!("fixture transport");
    };
    // Explicit missing --config is a create target for add; a read still fails.
    result(cli(home.path(), &path, &["list"]), None, false).await?;
    result(
        cli(home.path(), &path, &["add", "fixture", "--url", &url]),
        None,
        true,
    )
    .await?;
    result(cli(home.path(), &path, &["show", "fixture"]), None, true).await?;
    ensure!(
        upstream
            .received_requests()
            .await
            .context("requests")?
            .is_empty(),
        "save/read contacted MCP"
    );
    let checked = result(cli(home.path(), &path, &["check", "fixture"]), None, true).await?;
    ensure!(
        checked["servers"][0]["ok"] == true
            && checked["servers"][0]["tools"] == json!(["echo", "other"])
    );
    let requests = upstream.received_requests().await.context("requests")?;
    ensure!(requests.iter().any(|r| {
        serde_json::from_slice::<Value>(&r.body).is_ok_and(|v| v["method"] == "tools/list")
    }));
    let args_path = home.path().join("args.yaml");
    result(
        cli(
            home.path(),
            &args_path,
            &[
                "add",
                "local",
                "--command",
                "uninstalled-program",
                "--",
                "--flag",
                "two words",
            ],
        ),
        None,
        true,
    )
    .await?;
    let raw: Value = serde_saphyr::from_str(&std::fs::read_to_string(&args_path)?)?;
    ensure!(raw["mcp_servers"]["local"]["transport"]["args"] == json!(["--flag", "two words"]));
    let default = home.path().join("bitrouter.yaml");
    let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
    command
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("BITROUTER_HOME", home.path())
        .args(["mcp", "add", "default", "--command", "not-installed"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let receipt = result(command, None, true).await?;
    ensure!(receipt["source"] == default.to_string_lossy().as_ref() && default.is_file());
    Ok(())
}

#[tokio::test]
async fn unsupported_yaml_and_prefix_conflicts_fail_without_rewriting() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("config.yaml");
    for original in [
        "{inherit_defaults: false, mcp_servers: {}}\n",
        "inherit_defaults: false\nmcp_servers: &shared {}\n",
        "inherit_defaults: false\nmcp_servers:\n  old:\n    name: old\n    transport: {type: stdio, command: old}\n    tool_prefix: same__\n",
    ] {
        std::fs::write(&path, original)?;
        result(
            cli(home.path(), &path, &["add", "new", "--from", "-"]),
            Some("name: new\ntransport: {type: stdio, command: new}\ntool_prefix: same__\n"),
            false,
        )
        .await?;
        ensure!(std::fs::read_to_string(&path)? == original);
    }
    Ok(())
}

#[tokio::test]
async fn shortcut_conflicts_and_relative_paths_have_explicit_cli_contracts() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("config.yaml");
    let original = "inherit_defaults: false\n";
    std::fs::write(&path, original)?;
    for args in [
        vec![
            "add",
            "one",
            "--url",
            "https://example.com",
            "--command",
            "no-run",
        ],
        vec![
            "add",
            "one",
            "--url",
            "https://example.com",
            "--from",
            "missing.yaml",
        ],
        vec![
            "add",
            "one",
            "--command",
            "no-run",
            "--from",
            "missing.yaml",
        ],
        vec!["add", "one", "--", "orphan-argument"],
    ] {
        let output = cli(home.path(), &path, &args).output().await?;
        ensure!(
            !output.status.success(),
            "conflicting/partial transport accepted"
        );
        ensure!(std::fs::read_to_string(&path)? == original);
    }
    let receipt = result(
        cli(
            home.path(),
            Path::new("config.yaml"),
            &["add", "one", "--url", "https://example.com"],
        ),
        None,
        true,
    )
    .await?;
    let reported = Path::new(receipt["source"].as_str().context("reported path")?);
    ensure!(
        reported.is_absolute() && std::fs::canonicalize(reported)? == std::fs::canonicalize(&path)?
    );
    for command in ["list", "show", "add", "edit", "remove", "check"] {
        let output = Command::new(env!("CARGO_BIN_EXE_bro"))
            .args(["mcp", command, "--help"])
            .output()
            .await?;
        ensure!(output.status.success(), "help failed for {command}");
        ensure!(!output.stdout.contains(&0x1b));
    }
    let help = Command::new(env!("CARGO_BIN_EXE_bro"))
        .args(["mcp", "--help"])
        .output()
        .await?;
    let help = String::from_utf8(help.stdout)?;
    for command in ["list", "show", "add", "edit", "remove", "check"] {
        ensure!(
            help.lines()
                .any(|line| line.trim_start().starts_with(&format!("{command} ")))
        );
    }
    ensure!(
        !help
            .lines()
            .any(|line| line.trim_start().starts_with("search "))
    );
    Ok(())
}
