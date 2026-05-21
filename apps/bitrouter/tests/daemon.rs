//! Integration tests for the Unix-socket daemon control surface:
//! roundtrip `Status` / `Route` / `Reload` / `Stop` against a fully assembled
//! `App`. Bare-bones — no HTTP server, just the control socket.

use std::sync::Arc;
use std::time::Duration;

use bitrouter::build_app_with_path;
use bitrouter::daemon::{self, DaemonCommand, DaemonResponse, NoopReloader};
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
/// We deliberately use `/tmp` rather than `std::env::temp_dir()` (which is
/// `$TMPDIR` = `/var/folders/.../T/` on macOS, ~48 chars by itself). Unix
/// domain socket paths are capped at `SUN_LEN` (104 bytes on macOS, 108 on
/// Linux); the long mac TMPDIR plus a nanosecond suffix plus `bitrouter.sock`
/// would overflow. `/tmp` keeps every test socket comfortably under the cap.
fn tempdir(tag: &str) -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp").join(format!(
        "brd-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
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
    ));

    // Wait for the listener to be ready (bind is fast but not synchronous).
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

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

    // Stop → server returns and the socket file is removed.
    let stop = daemon::send_command(&socket, &DaemonCommand::Stop)
        .await
        .unwrap();
    assert!(matches!(stop, DaemonResponse::Ok));
    server.await.unwrap().unwrap();
    assert!(
        !socket.exists(),
        "socket file should be removed on shutdown"
    );

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
    ));
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

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
/// provider catalog on a file reload. `openai` here is declared with no
/// `api_base` — it comes from the compiled-in catalog at assembly time.
/// If the reload path swapped in a bare file re-read (skipping
/// `apply_builtin_defaults`), the provider would come back with an empty
/// `api_base`, and an `auto_discover` provider would then silently drop
/// every model. The SDK's own `RoutingTable::reload` cannot fix this — it
/// sits below `bitrouter-providers` — so the reloader rebuilds the config
/// in the app layer.
#[tokio::test]
async fn reload_re_applies_builtin_provider_catalog() {
    use bitrouter::daemon::DaemonReloader;
    use bitrouter::reload::{AppReloader, ReloadSource};

    let dir = tempdir("reload-builtin");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let cfg_path = dir.join("bitrouter.yaml");
    // `openai` is a built-in: `api_base` is omitted and must be filled
    // from the catalog. Explicit `models` keep discovery off the network.
    let yaml = r#"
server:
  listen: "127.0.0.1:0"
  skip_auth: true
database:
  url: "sqlite::memory:"
inherit_defaults: true
providers:
  openai:
    api_key: k1
    models: [{ id: gpt-5 }]
"#;
    tokio::fs::write(&cfg_path, yaml).await.unwrap();

    let cfg = config::load(&cfg_path).await.unwrap();
    let assembled = build_app_with_path(&cfg, Some(&cfg_path)).await.unwrap();

    // Sanity: assembly already filled the catalog `api_base`.
    assert_eq!(
        assembled.routing_table.snapshot_config().providers["openai"].api_base,
        "https://api.openai.com/v1",
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
    let openai = after.providers.get("openai").expect("openai still present");
    assert_eq!(
        openai.api_base, "https://api.openai.com/v1",
        "built-in `api_base` must survive a file reload",
    );
    assert!(
        !openai.api_protocol.is_empty(),
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
    ));
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

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
    ));
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

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
    ));
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Send garbage directly — bypass send_command's JSON serialisation.
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;
        let mut s = BufReader::new(UnixStream::connect(&socket).await.unwrap());
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
    ));
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

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
    ));
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

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
