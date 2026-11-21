use super::*;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
#[path = "../../tests/support/acp.rs"]
mod support;
use support::fixture;

const TOKEN: &str = "brk_synthetic-acp-bound-token";

async fn invoke(
    agent: &str,
    endpoint: &str,
    flag: Option<&str>,
    prompt: &str,
    workspace: &Path,
    duration: Duration,
) -> Result<(i32, String)> {
    let harness = catalog::find(agent)?;
    let plan = Endpoint::new(harness, endpoint, TOKEN)?;
    let home = tempfile::tempdir()?;
    let mut inherited = Command::new(fixture()?);
    inherited
        .env("OPENAI_API_KEY", "provider-sentinel")
        .env("BRO_FIXTURE_PARENT_SECRET", "parent-sentinel");
    let mut cmd = agent_command(
        inherited,
        harness,
        &plan,
        Some("fixture-model"),
        home.path(),
    );
    cmd.arg("--check-model");
    if let Some(arg) = flag {
        cmd.arg(arg);
    }
    let mut out = Vec::new();
    let result = execute(cmd, &plan, TOKEN, workspace, prompt, duration, &mut out).await?;
    Ok((result, String::from_utf8(out)?))
}

#[tokio::test]
async fn bound_model_calls_use_both_pinned_contracts() -> Result<()> {
    // A cold Cargo fixture build is outside the mock gateway request deadline.
    fixture()?;
    for (agent, capability, path) in [
        ("codex", None, "/company/v1/responses"),
        ("claude", Some("--fallback"), "/company/v1/messages"),
    ] {
        let workspace = tempfile::tempdir()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let endpoint = format!("http://{}/company/v1/", listener.local_addr()?);
        let gateway = tokio::task::spawn_blocking(move || -> Result<()> {
            let start = std::time::Instant::now();
            let mut conn = loop {
                match listener.accept() {
                    Ok((conn, _)) => break conn,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && start.elapsed() < Duration::from_secs(10) =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            conn.set_read_timeout(Some(Duration::from_secs(2)))?;
            let mut reader = BufReader::new(&conn);
            let mut request = String::new();
            reader.read_line(&mut request)?;
            ensure!(
                request.trim_end() == format!("POST {path} HTTP/1.1"),
                "wrong model endpoint"
            );
            let mut authorized = false;
            loop {
                let mut line = String::new();
                ensure!(reader.read_line(&mut line)? != 0, "incomplete request");
                if line == "\r\n" {
                    break;
                }
                authorized |= line.trim_end() == format!("Authorization: Bearer {TOKEN}");
            }
            ensure!(authorized, "wrong gateway credential");
            reader.read_exact(&mut [0; 2])?;
            conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
            Ok(())
        });
        let (code, output) = invoke(
            agent,
            &endpoint,
            capability,
            "call gateway",
            workspace.path(),
            Duration::from_secs(5),
        )
        .await?;
        gateway.await??;
        assert_eq!(code, 0);
        assert!(!output.contains(TOKEN));
        let lines = output
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["type"], "message_chunk");
        assert_eq!(lines[0]["text"], "adapter reply [REDACTED]");
        assert_eq!(
            lines[1],
            serde_json::json!({"type":"result","stop_reason":"end_turn"})
        );
    }
    Ok(())
}

#[tokio::test]
async fn provider_mismatch_refuses_before_prompt_and_errors_hide_auth() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let err = invoke(
        "claude",
        "http://localhost:1",
        Some("--mismatch"),
        "unused",
        workspace.path(),
        Duration::from_secs(2),
    )
    .await
    .err()
    .context("wrong endpoint accepted")?;
    assert!(err.to_string().contains("did not confirm"));
    assert!(!workspace.path().join("prompt-received").exists());
    for (prompt, error) in [
        ("disconnect", "disconnected"),
        ("reject", "rejected"),
        ("hang", "timed out"),
    ] {
        let err = invoke(
            "claude",
            "http://localhost:1",
            None,
            prompt,
            workspace.path(),
            Duration::from_millis(400),
        )
        .await
        .err()
        .context("failed prompt accepted")?;
        assert!(!format!("{err:#}").contains(TOKEN));
        assert!(err.to_string().contains(error), "{prompt}: {err:#}");
    }
    Ok(())
}

#[tokio::test]
async fn denial_never_selects_an_allow_option() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    for prompt in ["ask", "only-allow"] {
        let (code, output) = invoke(
            "codex",
            "http://localhost:1",
            None,
            prompt,
            workspace.path(),
            Duration::from_secs(3),
        )
        .await?;
        assert_eq!(code, 5);
        assert!(output.contains("\"decision\":\"denied\""));
    }
    Ok(())
}

#[tokio::test]
async fn deadline_stops_the_package_runner_descendant() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let error = invoke(
        "codex",
        "http://localhost:1",
        None,
        "hang-with-child",
        workspace.path(),
        Duration::from_secs(2),
    )
    .await
    .err()
    .context("hanging prompt unexpectedly succeeded")?;
    assert!(error.to_string().contains("timed out"));
    assert!(workspace.path().join("descendant-ready").is_file());
    std::fs::write(workspace.path().join("release"), "release")?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!workspace.path().join("orphan-effect").exists());
    Ok(())
}

#[test]
fn invalid_or_unbound_launch_is_rejected() -> Result<()> {
    let h = catalog::find("claude")?;
    assert_eq!(
        Endpoint::new(h, "http://v1/v1/", TOKEN)?
            .set_provider()
            .base_url,
        "http://v1"
    );
    for endpoint in [
        "http://user@host",
        "http://host/?key=secret",
        "http://host/#secret",
        "file:///etc",
    ] {
        assert!(Endpoint::new(h, endpoint, TOKEN).is_err());
    }
    for token in ["", "abc\r\nInjected: yes"] {
        assert!(Endpoint::new(h, "http://localhost", token).is_err());
    }
    for h in catalog::CATALOG.iter().filter(|h| h.adapter.is_none()) {
        assert!(Endpoint::new(h, "http://localhost", TOKEN).is_err());
    }
    Ok(())
}
