//! Real routed usage through settlement, owner IPC, and the shipped CLI.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use axum_test::TestServer;
use bitrouter::actions::panel::PanelReport;
use bitrouter::daemon::{self, DaemonCommand, NoopObserveStatus, NoopReloader};
use bitrouter::metering::MeteringStore;
use bitrouter_sdk::config;
use bitrouter_sdk::server::{AppState, build_router};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn routed_requests_reach_client_session_panel_via_cli() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"panel-test", "object":"chat.completion", "model":"fixture",
            "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,
                "prompt_tokens_details":{"cached_tokens":3},
                "completion_tokens_details":{"reasoning_tokens":2}}
        })))
        .mount(&upstream).await;
    let yaml = format!(
        r#"
inherit_defaults: false
server:
  skip_auth: true
database:
  url: 'sqlite::memory:'
providers:
  fixture:
    api_base: {}
    api_key: panel-private-fixture-key
    models: [{{ id: fixture }}]
"#,
        upstream.uri()
    );
    let config = config::parse(&yaml)?;
    let config_path = directory.path().join("bitrouter.yaml");
    tokio::fs::write(&config_path, &yaml).await?;
    let assembled = bitrouter::build_app_with_path(&config, Some(&config_path)).await?;
    let gateway = TestServer::new(build_router(AppState {
        language_model: assembled
            .app
            .language_model()
            .context("pipeline missing")?
            .clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    }));
    let since = chrono::Utc::now() - chrono::Duration::minutes(1);
    for (header, session) in [
        ("session-id", "root-a"),
        ("session-id", "root-b"),
        ("x-claude-code-session-id", "root-a"),
    ] {
        let response = gateway
            .post("/v1/chat/completions")
            .add_header(header, session)
            .json(
                &json!({"model":"fixture","messages":[{"role":"user","content":"private prompt"}]}),
            )
            .await;
        ensure!(
            response.status_code().is_success(),
            "fixture route failed: {}",
            response.text()
        );
    }
    let until = chrono::Utc::now() + chrono::Duration::seconds(1);
    let socket = directory.path().join("panel.sock");
    let server = tokio::spawn(daemon::run_control_socket(
        socket.clone(),
        Arc::new(assembled.app),
        "127.0.0.1:0".into(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
        MeteringStore::new(assembled.db),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if daemon::probe_status(&socket).await?.is_some() {
                break Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"))
        .args([
            "panel",
            "--since",
            &since.to_rfc3339(),
            "--until",
            &until.to_rfc3339(),
            "--session-limit",
            "1",
            "--socket",
        ])
        .arg(&socket)
        .output()
        .await?;
    // Always stop the fixture even when the CLI contract fails.
    daemon::send_command(&socket, &DaemonCommand::Stop).await?;
    server.await??;
    ensure!(
        output.status.success(),
        "panel CLI failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report: PanelReport = serde_json::from_slice(&output.stdout)?;
    ensure!(report.schema_version == 1);
    ensure!(
        report.clients.len() == 2,
        "expected two observed clients: {:?}",
        report.clients
    );
    let codex = report
        .clients
        .iter()
        .find(|client| client.label == "Codex")
        .context("Codex absent")?;
    let claude = report
        .clients
        .iter()
        .find(|client| client.label == "Claude Code")
        .context("Claude absent")?;
    ensure!(
        codex.tokens.value == Some(28),
        "cache/reasoning were double counted or usage lost"
    );
    ensure!(claude.tokens.value == Some(14));
    ensure!(codex.sessions.len() == 1 && report.session_page.next_offset == Some(1));
    ensure!(codex.sessions[0].tokens.value == Some(14));
    let serialized = String::from_utf8(output.stdout)?;
    ensure!(!serialized.contains("panel-private-fixture-key"));
    ensure!(!serialized.contains("private prompt"));
    ensure!(
        !serialized.contains("root-a"),
        "raw session identities crossed UI contract"
    );
    Ok(())
}

#[tokio::test]
async fn invalid_panel_day_fails_before_connecting() -> Result<()> {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"))
        .args([
            "panel",
            "--since",
            "2026-09-17T00:00:00Z",
            "--until",
            "2026-09-19T00:00:00Z",
        ])
        .output()
        .await?;
    ensure!(!output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        envelope["error"]["message"]
            .as_str()
            .is_some_and(|s| s.contains("26 hours"))
    );
    Ok(())
}

/// Closing a native menu cancels its CLI child. A lost reply must stay local
/// to that connection, including when the request was malformed.
#[cfg(unix)]
#[tokio::test]
async fn cancelled_panel_reads_do_not_stop_the_daemon() -> Result<()> {
    use std::io::Write;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

    let directory = tempfile::Builder::new()
        .prefix("br-panel-")
        .tempdir_in("/tmp")?;
    let config_path = directory.path().join("bitrouter.yaml");
    let yaml = "inherit_defaults: false\nserver:\n  skip_auth: true\ndatabase:\n  url: 'sqlite::memory:'\n";
    tokio::fs::write(&config_path, yaml).await?;
    let config = config::parse(yaml)?;
    let assembled = bitrouter::build_app_with_path(&config, Some(&config_path)).await?;
    let socket = directory.path().join("panel.sock");
    let server = tokio::spawn(daemon::run_control_socket(
        socket.clone(),
        Arc::new(assembled.app),
        "127.0.0.1:0".into(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
        MeteringStore::new(assembled.db),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while daemon::probe_status(&socket).await?.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<(), anyhow::Error>(())
    })
    .await??;
    let until = chrono::Utc::now();
    let command = serde_json::to_string(&DaemonCommand::Panel {
        input: bitrouter::actions::panel::PanelInput {
            since: until - chrono::Duration::hours(1),
            until,
            session_limit: 100,
            session_offset: 0,
        },
    })?;
    let abandon = |request: &str| -> Result<()> {
        // Synchronous writes avoid yielding to the single-threaded server
        // until the read side is already closed, making the lost reply certain.
        let mut stream = UnixStream::connect(&socket)?;
        writeln!(stream, "{request}")?;
        stream.shutdown(Shutdown::Both)?;
        Ok(())
    };
    for request in [&command, "{invalid-json"] {
        abandon(request)?;
        let status =
            tokio::time::timeout(Duration::from_secs(5), daemon::probe_status(&socket)).await??;
        ensure!(status.is_some(), "abandoned read terminated the daemon");
    }
    // An explicitly accepted Stop still works even if the CLI goes away.
    abandon("{\"cmd\":\"stop\"}")?;
    tokio::time::timeout(Duration::from_secs(5), server).await???;
    Ok(())
}
