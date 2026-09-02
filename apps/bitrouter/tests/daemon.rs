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

/// The ignored-config guard must reach the operator on the `serve` path, and
/// the only way it can is by being *carried out* of assembly rather than
/// logged inside it: `build_app_with_path` runs before
/// `init_serve_tracing_subscriber`, so a `tracing::warn!` in there is emitted
/// into a process with no subscriber installed and is dropped silently.
///
/// That is exactly how the first version of this guard shipped-and-did-nothing
/// in local testing, so the regression is worth pinning.
///
/// **Be precise about what this does and does not pin.** It asserts the
/// warnings are *carried out* of assembly. It does **not** assert that nothing
/// was logged during assembly, so re-adding a `tracing::warn!` inside
/// `build_app_with_path` would still pass here. That assertion was considered
/// and rejected as unreliable in the wrong direction: `tracing`'s per-callsite
/// `Interest` cache is process-global, so a sibling test in this binary hitting
/// the same callsite with no subscriber installed caches `Interest::never` —
/// and a "nothing was logged" assertion then passes vacuously, which is exactly
/// the failure it exists to catch. `crates/bitrouter-telemetry/tests/ingress_log_target.rs`
/// is a separate binary for the same reason; the difference is that its
/// assertion is positive, so a vacuous run fails rather than passes.
#[tokio::test]
async fn ignored_config_is_carried_out_of_assembly_not_logged_inside_it() {
    let dir = tempdir("ignored-config");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let path = dir.join("bitrouter.yaml");
    tokio::fs::write(
        &path,
        format!(
            "{}\nplugins:\n  bitrouter-observe:\n    otel:\n      endpoint: http://collector:4318\n",
            tiny_config_yaml("sqlite::memory:")
        ),
    )
    .await
    .unwrap();
    let cfg = config::load(&path).await.unwrap();

    // What the guard produces for this config, computed independently of
    // assembly. Asserting against this rather than against a substring makes
    // the second assertion exact: assembly must carry these out *verbatim*,
    // not merely emit something that happens to mention the stale id.
    let expected = bitrouter::assemble::ignored_config_warnings(&cfg);
    assert!(
        expected
            .iter()
            .any(|line| line.contains("plugins.bitrouter-observe")),
        "the guard must flag the stale plugin id, got: {expected:?}"
    );

    let assembled = build_app_with_path(&cfg, Some(&path)).await.unwrap();
    // Deliberately not `assert_eq!`, and deliberately printing only `expected`.
    // `Assembled` also carries `continuation_registry`, which lazily derives
    // from installation key material, and CodeQL's taint tracking is
    // field-insensitive across the struct — so formatting *any* field off
    // `assembled` reads as logging a secret (rule `rust/cleartext-logging`,
    // alert 43). It is a false positive: `ignored_config` is the first thing
    // `build_app_with_path` computes, purely from `config`, before a key is
    // touched. Printing the untracked side costs nothing and keeps the alert
    // honest for the day it catches something real.
    assert!(
        assembled.ignored_config == expected,
        "assembly must carry the guard's warnings out rather than logging them; expected {expected:?}"
    );
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
        None,
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
async fn authenticated_acp_route_state_roundtrips_over_the_control_socket() {
    let dir = tempdir("acp-route");
    let cfg_path = write_config(&dir, "sqlite::memory:").await;
    let cfg = config::load(&cfg_path).await.unwrap();
    let assembled = build_app_with_path(&cfg, Some(&cfg_path)).await.unwrap();
    let runtime = assembled.acp_runtime.clone();
    let app = Arc::new(assembled.app);
    let socket = dir.join("bitrouter.sock");
    let server = tokio::spawn(daemon::run_control_socket_with_acp_runtime(
        socket.clone(),
        app,
        "127.0.0.1:1234".to_string(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
        None,
        runtime.clone(),
    ));
    wait_until_ready(&socket).await;

    let issued = daemon::send_command(
        &socket,
        &DaemonCommand::AcpControllerIssue {
            controller_instance_id: "brc_test".to_string(),
        },
    )
    .await
    .unwrap();
    let credential = match issued {
        DaemonResponse::AcpControllerCredential {
            controller_instance_id,
            credential,
            ..
        } => {
            assert_eq!(controller_instance_id, "brc_test");
            credential
        }
        other => panic!("expected ACP credential, got {other:?}"),
    };
    assert_eq!(
        runtime
            .authenticate(credential.as_str())
            .unwrap()
            .controller_instance_id(),
        "brc_test"
    );

    let set = daemon::send_command(
        &socket,
        &DaemonCommand::AcpRouteSet {
            controller_instance_id: "brc_test".to_string(),
            session_id: "native-session".to_string(),
            route: "gpt-5".to_string(),
        },
    )
    .await
    .unwrap();
    match set {
        DaemonResponse::AcpRouteState {
            available,
            current,
            scope,
        } => {
            assert!(available.contains(&"gpt-5".to_string()));
            assert_eq!(current.as_deref(), Some("gpt-5"));
            assert_eq!(scope, "session");
        }
        other => panic!("expected ACP route state, got {other:?}"),
    }

    let invalid = daemon::send_command(
        &socket,
        &DaemonCommand::AcpRouteSet {
            controller_instance_id: "brc_test".to_string(),
            session_id: "native-session".to_string(),
            route: "missing-model".to_string(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(invalid, DaemonResponse::Error { .. }));
    assert_eq!(
        runtime
            .current_route("brc_test", "native-session")
            .unwrap()
            .route(),
        "gpt-5"
    );

    let other = daemon::send_command(
        &socket,
        &DaemonCommand::AcpRouteList {
            controller_instance_id: "brc_other".to_string(),
            session_id: "native-session".to_string(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(other, DaemonResponse::Error { .. }));

    let reset = daemon::send_command(
        &socket,
        &DaemonCommand::AcpRouteReset {
            controller_instance_id: "brc_test".to_string(),
            session_id: "native-session".to_string(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        reset,
        DaemonResponse::AcpRouteState {
            current: None,
            scope,
            ..
        } if scope == "default"
    ));

    assert!(matches!(
        daemon::send_command(
            &socket,
            &DaemonCommand::AcpControllerRevoke {
                controller_instance_id: "brc_test".to_string(),
            },
        )
        .await
        .unwrap(),
        DaemonResponse::Ok
    ));
    assert!(runtime.authenticate(credential.as_str()).is_none());

    let _ = daemon::send_command(&socket, &DaemonCommand::Stop).await;
    server.await.unwrap().unwrap();
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
        None,
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
        None,
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
        assembled.upstream_executor.clone(),
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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

/// `providers/set`'s substrate: a `SetRoute` command over the control socket
/// must move the traffic of the launch that asked, and only that launch.
///
/// This is the half the ACP surface could not do on its own — the substrate is
/// a separate process and the agent child talks to the daemon directly, so
/// without this the picker would be a control that does not do what it looks
/// like it does.
#[tokio::test]
async fn set_route_reroutes_only_the_named_launch() {
    use bitrouter::policy_table_router::{PolicyTable, PolicyTableRouter};
    use bitrouter_sdk::{HeaderMap, PromptTransform};

    let dir = tempdir("setroute");
    let cfg_path = write_config(&dir, "sqlite::memory:").await;
    let cfg = config::load(&cfg_path).await.unwrap();
    let assembled = build_app_with_path(&cfg, Some(&cfg_path)).await.unwrap();
    let app = Arc::new(assembled.app);

    // The same handle the daemon holds and the live transform reads.
    let router = Arc::new(PolicyTableRouter::new(PolicyTable::inert()));

    let socket = dir.join("bitrouter.sock");
    let server = tokio::spawn(daemon::run_control_socket(
        socket.clone(),
        app.clone(),
        "127.0.0.1:1234".to_string(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
        Some(router.clone()),
    ));
    wait_until_ready(&socket).await;

    let routed_model = |auth: &str| {
        let mut headers = HeaderMap::new();
        if let Ok(value) = auth.parse() {
            headers.insert("authorization", value);
        }
        let mut prompt = bitrouter_sdk::language_model::types::Prompt {
            model: "gpt-5".to_string(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![bitrouter_sdk::language_model::types::Message::text(
                bitrouter_sdk::language_model::types::Role::User,
                "hi",
            )],
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        router.apply_with_headers(&mut prompt, &headers);
        prompt.model
    };

    // Before: the daemon serves the configured route.
    assert_eq!(routed_model("Bearer brl_mine"), "gpt-5");

    let resp = daemon::send_command(
        &socket,
        &DaemonCommand::SetRoute {
            launch_id: "brl_mine".to_string(),
            provider_id: Some("shared".to_string()),
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(resp, DaemonResponse::Ok),
        "SetRoute should succeed, got {resp:?}"
    );

    // After: that launch reaches the new provider, keeping its model…
    assert_eq!(routed_model("Bearer brl_mine"), "shared:gpt-5");
    // …and nobody else moves.
    assert_eq!(routed_model("Bearer brl_theirs"), "gpt-5");
    assert_eq!(routed_model("Bearer sk-a-real-key"), "gpt-5");

    // Clearing it restores the configured route.
    let resp = daemon::send_command(
        &socket,
        &DaemonCommand::SetRoute {
            launch_id: "brl_mine".to_string(),
            provider_id: None,
        },
    )
    .await
    .unwrap();
    assert!(matches!(resp, DaemonResponse::Ok), "{resp:?}");
    assert_eq!(routed_model("Bearer brl_mine"), "gpt-5");

    daemon::send_command(&socket, &DaemonCommand::Stop)
        .await
        .unwrap();
    let _ = server.await;
}
