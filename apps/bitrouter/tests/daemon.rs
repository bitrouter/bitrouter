//! Integration tests for the daemon control surface: roundtrip
//! `Status` / `Route` / `Reload` / `Stop` against a fully assembled `App`.
//! Bare-bones — no HTTP server, just the control endpoint. The transport is
//! platform-specific (a Unix domain socket, or a Windows named pipe), but
//! these tests drive it through the platform-agnostic `daemon` API so they
//! run unchanged on both.

use std::sync::Arc;
use std::time::Duration;

use bitrouter::build_app_with_path;
use bitrouter::daemon::{self, DaemonCommand, DaemonResponse, NoopObserveStatus, NoopReloader};
use bitrouter_sdk::App;

/// A reloader that re-reads only the routing table. Used by the reload test —
/// production callers use the AppReloader in main.rs which also reloads the
/// policy store.
struct RoutingTableReloader(Arc<App>);

#[async_trait::async_trait]
impl daemon::DaemonReloader for RoutingTableReloader {
    async fn reload(&self) -> anyhow::Result<()> {
        if let Some(pipeline) = self.0.language_model() {
            pipeline.routing_table().reload().await?;
        }
        Ok(())
    }
}
use bitrouter_sdk::config;

fn tiny_config_yaml(db_url: &str) -> String {
    // Two providers declare overlapping models so Route returns a real chain.
    format!(
        r#"
server:
  listen: "127.0.0.1:0"
  skip_auth: true
database:
  url: "{db_url}"
providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: k1
    models: [{{ id: gpt-5 }}, {{ id: shared }}]
  anthropic:
    api_base: https://api.anthropic.com/v1
    api_key: k2
    models: [{{ id: shared }}]
"#
    )
}

/// Write a tiny config to a temp file and return its path (so `build_app_with_path`
/// can record it for `reload`).
async fn write_config(dir: &std::path::Path, db_url: &str) -> std::path::PathBuf {
    tokio::fs::create_dir_all(dir).await.unwrap();
    let path = dir.join("bitrouter.yaml");
    tokio::fs::write(&path, tiny_config_yaml(db_url))
        .await
        .unwrap();
    path
}

/// Build a fresh tempdir scoped to this test run.
///
/// On Unix we deliberately use `/tmp` rather than `std::env::temp_dir()`
/// (which is `$TMPDIR` = `/var/folders/.../T/` on macOS, ~48 chars by itself).
/// Unix domain socket paths are capped at `SUN_LEN` (104 bytes on macOS, 108 on
/// Linux); the long mac TMPDIR plus a nanosecond suffix plus `bitrouter.sock`
/// would overflow. `/tmp` keeps every test socket comfortably under the cap.
/// On Windows the control endpoint is a named pipe (no path-length cap on the
/// backing file), so the platform temp dir is fine.
fn tempdir(tag: &str) -> std::path::PathBuf {
    #[cfg(unix)]
    let base = std::path::PathBuf::from("/tmp");
    #[cfg(not(unix))]
    let base = std::env::temp_dir();
    base.join(format!(
        "brd-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Wait until the daemon's control endpoint answers, so a test doesn't race
/// the listener's bind. Cross-platform: a connect failure (listener not up
/// yet) simply retries. `Status` is read-only, so probing with it is harmless.
async fn wait_until_ready(socket: &std::path::Path) {
    for _ in 0..100 {
        if daemon::send_command(socket, &DaemonCommand::Status)
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn status_route_and_stop_roundtrip_over_the_control_socket() {
    let dir = tempdir("status");
    let cfg_path = write_config(&dir, "sqlite::memory:").await;
    let cfg = config::load(&cfg_path).await.unwrap();
    let assembled = build_app_with_path(&cfg, Some(&cfg_path)).await.unwrap();
    let app = Arc::new(assembled.app);

    let socket = dir.join("bitrouter.sock");
    let server = tokio::spawn(daemon::run_control_socket(
        socket.clone(),
        app.clone(),
        "127.0.0.1:1234".to_string(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
    ));

    // Wait for the listener to be ready (bind is fast but not synchronous).
    wait_until_ready(&socket).await;

    // Status → reports a real model count from the routing table.
    let status = daemon::send_command(&socket, &DaemonCommand::Status)
        .await
        .unwrap();
    match status {
        DaemonResponse::Status { listen, models, .. } => {
            assert_eq!(listen, "127.0.0.1:1234");
            assert_eq!(models, 2, "gpt-5 + shared");
        }
        other => panic!("expected Status, got {other:?}"),
    }

    // Route → returns the cascade chain (anthropic first, then openai).
    let route = daemon::send_command(
        &socket,
        &DaemonCommand::Route {
            model: "shared".to_string(),
        },
    )
    .await
    .unwrap();
    match route {
        DaemonResponse::Route { chain } => {
            assert_eq!(chain.len(), 2);
            assert_eq!(chain[0].provider, "anthropic");
            assert_eq!(chain[1].provider, "openai");
        }
        other => panic!("expected Route, got {other:?}"),
    }

    // ObserveStatus → reports unwired (this test wires NoopObserveStatus).
    let observe = daemon::send_command(&socket, &DaemonCommand::ObserveStatus)
        .await
        .unwrap();
    match observe {
        DaemonResponse::ObserveStatus { payload } => {
            assert!(!payload.compiled_in, "test wired compiled_in: false");
            assert!(!payload.exporter_wired);
            assert!(payload.endpoint.is_none());
        }
        other => panic!("expected ObserveStatus, got {other:?}"),
    }

    // Stop → server returns and the control endpoint is released.
    let stop = daemon::send_command(&socket, &DaemonCommand::Stop)
        .await
        .unwrap();
    assert!(matches!(stop, DaemonResponse::Ok));
    server.await.unwrap().unwrap();
    assert!(
        !daemon::endpoint_in_use(&socket),
        "control endpoint should be released on shutdown"
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn probe_status_reports_ready_when_daemon_is_up() {
    let dir = tempdir("probe-up");
    let cfg_path = write_config(&dir, "sqlite::memory:").await;
    let cfg = config::load(&cfg_path).await.unwrap();
    let assembled = build_app_with_path(&cfg, Some(&cfg_path)).await.unwrap();
    let app = Arc::new(assembled.app);

    let socket = dir.join("bitrouter.sock");
    let server = tokio::spawn(daemon::run_control_socket(
        socket.clone(),
        app.clone(),
        "127.0.0.1:1234".to_string(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
    ));
    wait_until_ready(&socket).await;

    // A daemon is up → the probe returns its self-report.
    let info = daemon::probe_status(&socket)
        .await
        .unwrap()
        .expect("probe should see the running daemon");
    assert_eq!(info.listen, "127.0.0.1:1234");
    assert_eq!(info.models, 2, "gpt-5 + shared");

    let stop = daemon::send_command(&socket, &DaemonCommand::Stop)
        .await
        .unwrap();
    assert!(matches!(stop, DaemonResponse::Ok));
    server.await.unwrap().unwrap();
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn probe_status_reports_none_when_nothing_listens() {
    let dir = tempdir("probe-down");
    // The socket path is never bound — the probe must classify this as
    // "not reachable" (Ok(None)), not an error.
    let socket = dir.join("bitrouter.sock");
    assert!(daemon::probe_status(&socket).await.unwrap().is_none());
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn reload_re_reads_the_config_file() {
    let dir = tempdir("reload");
    let cfg_path = write_config(&dir, "sqlite::memory:").await;
    let cfg = config::load(&cfg_path).await.unwrap();
    let assembled = build_app_with_path(&cfg, Some(&cfg_path)).await.unwrap();
    let app = Arc::new(assembled.app);

    let socket = dir.join("bitrouter.sock");
    let server = tokio::spawn(daemon::run_control_socket(
        socket.clone(),
        app.clone(),
        "127.0.0.1:0".to_string(),
        Arc::new(RoutingTableReloader(app.clone())),
        Arc::new(NoopObserveStatus { compiled_in: false }),
    ));
    wait_until_ready(&socket).await;

    // Rewrite the config to drop the anthropic provider.
    let new_yaml = r#"
server:
  listen: "127.0.0.1:0"
  skip_auth: true
database:
  url: "sqlite::memory:"
providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: k1
    models: [{ id: gpt-5 }, { id: shared }]
"#;
    tokio::fs::write(&cfg_path, new_yaml).await.unwrap();

    let resp = daemon::send_command(&socket, &DaemonCommand::Reload { env: Vec::new() })
        .await
        .unwrap();
    assert!(matches!(resp, DaemonResponse::Ok));

    // After reload, `shared` resolves to one hop (openai), not two.
    let route = daemon::send_command(
        &socket,
        &DaemonCommand::Route {
            model: "shared".to_string(),
        },
    )
    .await
    .unwrap();
    match route {
        DaemonResponse::Route { chain } => {
            assert_eq!(chain.len(), 1, "anthropic should be gone after reload");
            assert_eq!(chain[0].provider, "openai");
        }
        other => panic!("expected Route, got {other:?}"),
    }

    // Cleanup
    let _ = daemon::send_command(&socket, &DaemonCommand::Stop).await;
    let _ = server.await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

/// Regression: the production `AppReloader` must re-apply the built-in
/// provider catalog on a file reload. The `bitrouter` cloud gateway here is
/// declared with no `api_base` — it is the one compiled-in built-in, filled
/// from the catalog at assembly time. (The other known providers come from the
/// fetched registry, not a compiled-in snapshot.) If the reload path swapped in
/// a bare file re-read (skipping `apply_builtin_defaults`), the provider would
/// come back with an empty `api_base`. The SDK's own `RoutingTable::reload`
/// cannot fix this — it sits below `bitrouter-providers` — so the reloader
/// rebuilds the config in the app layer.
#[tokio::test]
async fn reload_re_applies_builtin_provider_catalog() {
    use bitrouter::daemon::DaemonReloader;
    use bitrouter::reload::{AppReloader, ReloadSource};

    let dir = tempdir("reload-builtin");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let cfg_path = dir.join("bitrouter.yaml");
    // `bitrouter` is the compiled-in cloud gateway: `api_base` is omitted and
    // must be filled from the catalog. Explicit `models` keep the canonical
    // backfill (and any discovery) off the network.
    let yaml = r#"
server:
  listen: "127.0.0.1:0"
  skip_auth: true
database:
  url: "sqlite::memory:"
inherit_defaults: true
providers:
  bitrouter:
    api_key: k1
    models: [{ id: gpt-5 }]
"#;
    tokio::fs::write(&cfg_path, yaml).await.unwrap();

    let cfg = config::load(&cfg_path).await.unwrap();
    let assembled = build_app_with_path(&cfg, Some(&cfg_path)).await.unwrap();

    // Sanity: assembly already filled the catalog `api_base`.
    assert_eq!(
        assembled.routing_table.snapshot_config().providers["bitrouter"].api_base,
        "https://api.bitrouter.ai/v1",
    );

    let reloader = AppReloader::new(
        assembled.policy_store.clone(),
        assembled.routing_table.clone(),
        ReloadSource::File(cfg_path.clone()),
    );
    reloader.reload().await.expect("reload succeeds");

    // The reloaded config must STILL carry the catalog `api_base` and
    // `api_protocol` — the reload re-applies `apply_builtin_defaults`,
    // not just a bare file re-read.
    let after = assembled.routing_table.snapshot_config();
    let gateway = after
        .providers
        .get("bitrouter")
        .expect("bitrouter still present");
    assert_eq!(
        gateway.api_base, "https://api.bitrouter.ai/v1",
        "built-in `api_base` must survive a file reload",
    );
    assert!(
        !gateway.api_protocol.is_empty(),
        "built-in `api_protocol` must survive a file reload",
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn route_for_unknown_model_returns_a_clean_error() {
    let dir = tempdir("noroute");
    let cfg_path = write_config(&dir, "sqlite::memory:").await;
    let cfg = config::load(&cfg_path).await.unwrap();
    let assembled = build_app_with_path(&cfg, Some(&cfg_path)).await.unwrap();
    let app = Arc::new(assembled.app);

    let socket = dir.join("bitrouter.sock");
    let server = tokio::spawn(daemon::run_control_socket(
        socket.clone(),
        app.clone(),
        "127.0.0.1:0".to_string(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
    ));
    wait_until_ready(&socket).await;

    let resp = daemon::send_command(
        &socket,
        &DaemonCommand::Route {
            model: "no-such-model".to_string(),
        },
    )
    .await
    .unwrap();
    match resp {
        DaemonResponse::Error { message } => {
            assert!(message.contains("no-such-model") || message.to_lowercase().contains("model"));
        }
        other => panic!("expected Error, got {other:?}"),
    }

    let _ = daemon::send_command(&socket, &DaemonCommand::Stop).await;
    let _ = server.await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn concurrent_clients_are_all_served() {
    // Two clients hit the same listener back-to-back; both must get answers.
    let dir = tempdir("concurrent");
    let cfg_path = write_config(&dir, "sqlite::memory:").await;
    let cfg = config::load(&cfg_path).await.unwrap();
    let assembled = build_app_with_path(&cfg, Some(&cfg_path)).await.unwrap();
    let app = Arc::new(assembled.app);

    let socket = dir.join("bitrouter.sock");
    let server = tokio::spawn(daemon::run_control_socket(
        socket.clone(),
        app.clone(),
        "127.0.0.1:0".to_string(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
    ));
    wait_until_ready(&socket).await;

    let s1 = socket.clone();
    let s2 = socket.clone();
    let a = tokio::spawn(async move { daemon::send_command(&s1, &DaemonCommand::Status).await });
    let b = tokio::spawn(async move {
        daemon::send_command(
            &s2,
            &DaemonCommand::Route {
                model: "shared".to_string(),
            },
        )
        .await
    });
    let r1 = a.await.unwrap().unwrap();
    let r2 = b.await.unwrap().unwrap();
    assert!(matches!(r1, DaemonResponse::Status { .. }));
    assert!(matches!(r2, DaemonResponse::Route { .. }));

    let _ = daemon::send_command(&socket, &DaemonCommand::Stop).await;
    let _ = server.await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn malformed_input_does_not_take_the_server_down() {
    let dir = tempdir("malformed");
    let cfg_path = write_config(&dir, "sqlite::memory:").await;
    let cfg = config::load(&cfg_path).await.unwrap();
    let assembled = build_app_with_path(&cfg, Some(&cfg_path)).await.unwrap();
    let app = Arc::new(assembled.app);

    let socket = dir.join("bitrouter.sock");
    let server = tokio::spawn(daemon::run_control_socket(
        socket.clone(),
        app.clone(),
        "127.0.0.1:0".to_string(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
    ));
    wait_until_ready(&socket).await;

    // Send garbage directly — bypass send_command's JSON serialisation.
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let stream = daemon::connect_control(&socket).await.unwrap();
        let mut s = BufReader::new(stream);
        s.get_mut().write_all(b"not-json-at-all\n").await.unwrap();
        s.get_mut().flush().await.unwrap();
        let mut line = String::new();
        s.read_line(&mut line).await.unwrap();
        assert!(
            line.contains("error"),
            "expected an Error response, got: {line}"
        );
        assert!(
            line.contains("invalid command"),
            "should explain the parse failure"
        );
    }

    // The server must still be serving — issue a valid command after the bad one.
    let resp = daemon::send_command(&socket, &DaemonCommand::Status)
        .await
        .unwrap();
    assert!(matches!(resp, DaemonResponse::Status { .. }));

    let _ = daemon::send_command(&socket, &DaemonCommand::Stop).await;
    let _ = server.await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn reload_returns_error_when_the_config_is_broken() {
    let dir = tempdir("badyaml");
    let cfg_path = write_config(&dir, "sqlite::memory:").await;
    let cfg = config::load(&cfg_path).await.unwrap();
    let assembled = build_app_with_path(&cfg, Some(&cfg_path)).await.unwrap();
    let app = Arc::new(assembled.app);

    let socket = dir.join("bitrouter.sock");
    let server = tokio::spawn(daemon::run_control_socket(
        socket.clone(),
        app.clone(),
        "127.0.0.1:0".to_string(),
        Arc::new(RoutingTableReloader(app.clone())),
        Arc::new(NoopObserveStatus { compiled_in: false }),
    ));
    wait_until_ready(&socket).await;

    // Corrupt the config on disk.
    tokio::fs::write(&cfg_path, "this: is: not: valid: yaml: [{")
        .await
        .unwrap();

    let resp = daemon::send_command(&socket, &DaemonCommand::Reload { env: Vec::new() })
        .await
        .unwrap();
    match resp {
        DaemonResponse::Error { message } => {
            assert!(
                message.to_lowercase().contains("reload failed"),
                "expected 'reload failed' prefix, got: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
    // And the server is still alive afterwards.
    let resp = daemon::send_command(&socket, &DaemonCommand::Status)
        .await
        .unwrap();
    assert!(matches!(resp, DaemonResponse::Status { .. }));

    let _ = daemon::send_command(&socket, &DaemonCommand::Stop).await;
    let _ = server.await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

// Unix-only: this asserts the `0600` file mode. On Windows the control
// endpoint is a named pipe whose default security descriptor already restricts
// access to the creating user and administrators — there is no file mode to
// check, so the test does not apply.
#[cfg(unix)]
#[tokio::test]
async fn socket_file_has_owner_only_permissions() {
    // Anyone-on-the-host shouldn't be able to talk to our daemon. Verify the
    // socket is mode 0600 after bind.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir("perms");
    let cfg_path = write_config(&dir, "sqlite::memory:").await;
    let cfg = config::load(&cfg_path).await.unwrap();
    let assembled = build_app_with_path(&cfg, Some(&cfg_path)).await.unwrap();
    let app = Arc::new(assembled.app);

    let socket = dir.join("bitrouter.sock");
    let server = tokio::spawn(daemon::run_control_socket(
        socket.clone(),
        app.clone(),
        "127.0.0.1:0".to_string(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
    ));
    wait_until_ready(&socket).await;

    let meta = tokio::fs::metadata(&socket).await.unwrap();
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "control socket must be 0600, got {mode:o}");

    let _ = daemon::send_command(&socket, &DaemonCommand::Stop).await;
    let _ = server.await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn client_fails_clearly_when_no_daemon_is_listening() {
    // Path that definitely doesn't exist.
    let bogus = std::env::temp_dir().join(format!(
        "no-bitrouter-{}.sock",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let err = daemon::send_command(&bogus, &DaemonCommand::Status)
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("daemon running") || msg.contains("connecting to"),
        "expected a helpful error, got: {msg}"
    );
}
