//! Daemon control over a Unix domain socket.
//!
//! A running `bitrouter serve` listens on a control socket alongside the HTTP
//! API. The CLI's `stop` / `restart` / `reload` / `status` / `route`
//! subcommands are thin clients that connect, send one newline-delimited JSON
//! [`DaemonCommand`], and read one [`DaemonResponse`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use bitrouter_sdk::App;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::RoutingPrefs;

/// Anything the daemon's `Reload` command (and SIGHUP) should re-read. The
/// runtime reloader fans out to every reloadable subsystem — routing table,
/// policy store, … — atomically per subsystem. A failure in one is reported
/// but does not abort the others. The trait uses `#[async_trait]` so it is
/// object-safe (`Arc<dyn DaemonReloader>`).
#[async_trait::async_trait]
pub trait DaemonReloader: Send + Sync {
    /// Reload every reloadable subsystem.
    async fn reload(&self) -> anyhow::Result<()>;
}

/// A reloader that does nothing — useful for tests / minimal embeddings of the
/// daemon control surface that don't have anything to reload.
pub struct NoopReloader;

#[async_trait::async_trait]
impl DaemonReloader for NoopReloader {
    async fn reload(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A command sent from the CLI to a running daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum DaemonCommand {
    /// Stop the daemon — it finishes the response, then exits.
    Stop,
    /// Hot-reload the config / routing table. The CLI piggybacks a
    /// snapshot of API-key-style env vars from its own process so a
    /// `export OPENAI_API_KEY=…; bitrouter reload` propagates the new
    /// value into the running daemon without requiring a restart.
    /// `env` is `#[serde(default)]` for wire-compat with the
    /// historical unit variant — older clients sending `{"cmd":"reload"}`
    /// still deserialise as an empty override list.
    Reload {
        /// `(name, value)` pairs to apply to the daemon's env-override
        /// map before reload. Empty list = no override changes.
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// Report daemon status.
    Status,
    /// Resolve a model name through the live routing table.
    Route {
        /// The model name to resolve.
        model: String,
    },
    /// Report the OTel exporter's current state — what's wired, current
    /// cardinality usage, in-flight span count. Returned as a JSON
    /// snapshot. The wire format is the same `ObserveStatusPayload` the
    /// CLI pretty-prints for `bitrouter observe status`.
    ObserveStatus,
}

/// One resolved hop of a route chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteHop {
    /// Provider id.
    pub provider: String,
    /// Service / model id at the provider.
    pub service_id: String,
    /// The wire protocol for the hop.
    pub api_protocol: String,
}

/// The daemon's reply to a [`DaemonCommand`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resp", rename_all = "snake_case")]
pub enum DaemonResponse {
    /// The command succeeded with no payload.
    Ok,
    /// Status payload.
    Status {
        /// The daemon's process id.
        pid: u32,
        /// The HTTP listen address.
        listen: String,
        /// Count of routable models.
        models: usize,
    },
    /// A resolved route chain.
    Route {
        /// The ordered fallback chain.
        chain: Vec<RouteHop>,
    },
    /// OTel exporter snapshot.
    ObserveStatus {
        /// The serialized exporter state.
        payload: ObserveStatusPayload,
    },
    /// The command failed.
    Error {
        /// Human-readable failure detail.
        message: String,
    },
}

/// Serializable snapshot of the OTel exporter's state, transported over
/// the daemon control socket. Fields mirror `bitrouter_observe::otel::OtelStatus`
/// — this module re-states the wire format so the daemon crate doesn't
/// need to depend on the observe crate's type when the `otel` feature is
/// off (and so the JSON shape stays stable if the observe-side type
/// ever moves).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveStatusPayload {
    /// Whether the `otel` feature was compiled in.
    pub compiled_in: bool,
    /// Whether an exporter is actually wired.
    pub exporter_wired: bool,
    /// OTLP/HTTP+protobuf endpoint.
    pub endpoint: Option<String>,
    /// Number of additional headers (count only — no credential leak).
    pub header_count: usize,
    /// Service name reported on the resource.
    pub service_name: Option<String>,
    /// Number of `OTEL_RESOURCE_ATTRIBUTES` entries merged in.
    pub resource_attribute_count: usize,
    /// Sampler kind (e.g. `parentbased_always_on`).
    pub sampler: Option<String>,
    /// Sampler ratio argument (only for `*_traceidratio`).
    pub sampler_arg: Option<f64>,
    /// Whether metrics export is enabled.
    pub metrics_enabled: bool,
    /// Distinct `api_key_id` values seen by the cardinality limiter.
    pub api_key_count: usize,
    /// Cardinality cap for the `api_key_id` metric dimension.
    pub api_key_cap: usize,
    /// Distinct `user_id` values seen.
    pub user_id_count: usize,
    /// Cardinality cap for the `user_id` metric dimension.
    pub user_id_cap: usize,
    /// In-flight spans currently tracked.
    pub active_spans: usize,
}

impl ObserveStatusPayload {
    /// Payload for "feature compiled in but no exporter wired" — used by
    /// the noop provider and by the CLI's stopped-daemon fallback.
    pub fn unwired(compiled_in: bool) -> Self {
        Self {
            compiled_in,
            exporter_wired: false,
            endpoint: None,
            header_count: 0,
            service_name: None,
            resource_attribute_count: 0,
            sampler: None,
            sampler_arg: None,
            metrics_enabled: false,
            api_key_count: 0,
            api_key_cap: 0,
            user_id_count: 0,
            user_id_cap: 0,
            active_spans: 0,
        }
    }
}

/// Source of truth for the OTel exporter's current state, *and* the
/// lifecycle hook the daemon's shutdown path calls to flush pending
/// telemetry before the runtime tears down. Mirrors the
/// [`DaemonReloader`] pattern: the daemon module holds a trait object,
/// the app's assembly layer provides the concrete impl (which reads
/// from — and drives — the running `OtelExporter`).
///
/// Status and shutdown share a trait because they share a singleton:
/// each `serve()` has exactly one observe provider, with one address
/// and one set of background tasks. Splitting them would duplicate the
/// `Arc<dyn …>` plumbing without buying separation of concerns.
#[async_trait::async_trait]
pub trait ObserveStatusProvider: Send + Sync {
    /// Snapshot what the exporter looks like right now.
    fn status(&self) -> ObserveStatusPayload;

    /// Flush in-flight telemetry and tear down the exporter's background
    /// tasks. Must be driven from an async context that allows
    /// `spawn_blocking`, because the underlying SDK shutdown is
    /// synchronous and needs a separate thread to park on while the
    /// `rt-tokio` worker drains the SDK's internal channels.
    ///
    /// Default impl is a no-op so providers that own nothing
    /// (the noop / test variants) don't need to implement it.
    async fn shutdown(&self) {}
}

/// Provider that reports "not wired" — used when no exporter is built
/// (no YAML opt-in, no env var). `compiled_in` is plumbed through so the
/// caller can still distinguish "feature off" from "feature on but no
/// config."
pub struct NoopObserveStatus {
    /// Whether the `otel` feature was compiled into the binary.
    pub compiled_in: bool,
}

#[async_trait::async_trait]
impl ObserveStatusProvider for NoopObserveStatus {
    fn status(&self) -> ObserveStatusPayload {
        ObserveStatusPayload::unwired(self.compiled_in)
    }
    // `shutdown` falls back to the trait's default no-op — there is no
    // exporter to flush.
}

/// The default control-socket path when the config does not set one.
/// Stored as a relative path so [`resolve_socket_path`] places it next
/// to the config file rather than in the daemon's working directory.
pub const DEFAULT_CONTROL_SOCKET: &str = "./bitrouter.sock";

/// Resolve `cfg.server.control_socket` against the config file's
/// directory. Absolute paths are returned as-is; relative paths are
/// joined onto the parent of `config_path`. This is what keeps `start`
/// (which binds the socket) and `status` / `stop` / `reload` (which
/// connect to it) agreeing on a single path regardless of where each
/// invocation's CWD happens to be — the daemon launched from `~` and
/// the `status` run from `/tmp` both resolve to the same file under
/// `~/.bitrouter/`.
pub fn resolve_socket_path(config_path: &Path, cfg_socket: &str) -> PathBuf {
    let raw = Path::new(cfg_socket);
    if raw.is_absolute() {
        return raw.to_path_buf();
    }
    // Strip a leading `./` so the joined path doesn't render as
    // `/a/b/./c` — `Path::join` is faithful, not normalising.
    let stripped = raw.strip_prefix("./").unwrap_or(raw);
    let parent = config_path.parent().filter(|p| !p.as_os_str().is_empty());
    match parent {
        Some(dir) => dir.join(stripped),
        None => stripped.to_path_buf(),
    }
}

/// True when `err` came from connecting to a Unix socket that nothing
/// is listening on — i.e. the daemon is stopped, not that something
/// else went wrong. Inspects the chain for an io error of kind
/// `NotFound` (socket file absent) or `ConnectionRefused` (file present
/// but no acceptor, e.g. stale after a crash).
pub fn is_not_reachable(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .map(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                )
            })
            .unwrap_or(false)
    })
}

// ===== server side =====

/// Run the control-socket listener until a `Stop` command is received (then it
/// returns) — run this alongside `App::serve` under a `tokio::select!`.
///
/// `listen` is the HTTP address (reported in `Status`); the socket is bound at
/// `socket_path` and removed on return.
pub async fn run_control_socket(
    socket_path: PathBuf,
    app: Arc<App>,
    listen: String,
    reloader: Arc<dyn DaemonReloader>,
    observe: Arc<dyn ObserveStatusProvider>,
) -> Result<()> {
    // A stale socket file from a crashed daemon would block the bind.
    let _ = tokio::fs::remove_file(&socket_path).await;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding control socket {}", socket_path.display()))?;
    // The control surface includes Stop / Reload — only the daemon owner may
    // reach it. UnixListener::bind respects the process umask (typically 022 →
    // 0755); tighten to 0600 explicitly so any other local user is excluded.
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        .await
        .with_context(|| format!("chmod 0600 {}", socket_path.display()))?;
    tracing::info!(socket = %socket_path.display(), "control socket listening (mode 0600)");

    let result = accept_loop(&listener, &app, &listen, &reloader, &observe).await;
    let _ = tokio::fs::remove_file(&socket_path).await;
    result
}

async fn accept_loop(
    listener: &UnixListener,
    app: &Arc<App>,
    listen: &str,
    reloader: &Arc<dyn DaemonReloader>,
    observe: &Arc<dyn ObserveStatusProvider>,
) -> Result<()> {
    loop {
        let (stream, _addr) = listener
            .accept()
            .await
            .context("accepting control-socket connection")?;
        // Handle one command per connection. A `Stop` ends the loop (and thus
        // the whole `serve`); any other command loops for the next client.
        if handle_connection(stream, app, listen, reloader, observe).await? {
            tracing::info!("stop command received — shutting down");
            return Ok(());
        }
    }
}

/// Handle one connection. Returns `Ok(true)` if it was a `Stop` command.
async fn handle_connection(
    stream: UnixStream,
    app: &Arc<App>,
    listen: &str,
    reloader: &Arc<dyn DaemonReloader>,
    observe: &Arc<dyn ObserveStatusProvider>,
) -> Result<bool> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(false); // client hung up without sending anything
    }
    let command: DaemonCommand = match serde_json::from_str(line.trim()) {
        Ok(c) => c,
        Err(e) => {
            write_response(
                reader.get_mut(),
                &DaemonResponse::Error {
                    message: format!("invalid command: {e}"),
                },
            )
            .await?;
            return Ok(false);
        }
    };

    let is_stop = matches!(command, DaemonCommand::Stop);
    let response = dispatch(command, app, listen, reloader, observe).await;
    write_response(reader.get_mut(), &response).await?;
    Ok(is_stop)
}

async fn dispatch(
    command: DaemonCommand,
    app: &Arc<App>,
    listen: &str,
    reloader: &Arc<dyn DaemonReloader>,
    observe: &Arc<dyn ObserveStatusProvider>,
) -> DaemonResponse {
    match command {
        DaemonCommand::Stop => DaemonResponse::Ok,
        DaemonCommand::Reload { env } => {
            // Apply the CLI's env snapshot first so file-mode YAML
            // `${VAR}` substitution and zero-config's "is this
            // provider's key set" check see the freshly-exported
            // values. Empty list = caller didn't ask us to update env;
            // we keep whatever was already in the override map.
            if !env.is_empty() {
                let map: std::collections::HashMap<String, String> = env.into_iter().collect();
                bitrouter_sdk::config::set_env_overrides(map);
                tracing::info!("env override map updated by reload");
            }
            match reloader.reload().await {
                Ok(()) => {
                    tracing::info!("reload succeeded");
                    DaemonResponse::Ok
                }
                Err(e) => DaemonResponse::Error {
                    message: format!("reload failed: {e}"),
                },
            }
        }
        DaemonCommand::Status => {
            let models = app
                .language_model()
                .map(|p| p.routing_table().list_models().len())
                .unwrap_or(0);
            DaemonResponse::Status {
                pid: std::process::id(),
                listen: listen.to_string(),
                models,
            }
        }
        DaemonCommand::Route { model } => {
            let Some(pipeline) = app.language_model() else {
                return DaemonResponse::Error {
                    message: "no language_model pipeline configured".to_string(),
                };
            };
            match pipeline
                .routing_table()
                .route_chain(&model, &RoutingPrefs::default(), &CallerContext::local())
                .await
            {
                Ok(chain) => DaemonResponse::Route {
                    chain: chain
                        .into_iter()
                        .map(|t| RouteHop {
                            provider: t.provider_name,
                            service_id: t.service_id,
                            api_protocol: format!("{:?}", t.api_protocol).to_lowercase(),
                        })
                        .collect(),
                },
                Err(e) => DaemonResponse::Error {
                    message: e.to_string(),
                },
            }
        }
        DaemonCommand::ObserveStatus => DaemonResponse::ObserveStatus {
            payload: observe.status(),
        },
    }
}

async fn write_response(stream: &mut UnixStream, response: &DaemonResponse) -> Result<()> {
    let mut json = serde_json::to_string(response).context("serialising daemon response")?;
    json.push('\n');
    stream.write_all(json.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

// ===== client side =====

/// Connect to a running daemon's control socket, send `command`, return its
/// response. Fails clearly if no daemon is listening.
pub async fn send_command(socket_path: &Path, command: &DaemonCommand) -> Result<DaemonResponse> {
    let stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "connecting to {} — is the daemon running? (`bitrouter start`)",
            socket_path.display()
        )
    })?;
    let mut reader = BufReader::new(stream);
    let mut json = serde_json::to_string(command).context("serialising command")?;
    json.push('\n');
    reader.get_mut().write_all(json.as_bytes()).await?;
    reader.get_mut().flush().await?;

    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("reading daemon response")?;
    if line.trim().is_empty() {
        anyhow::bail!("daemon closed the connection without responding");
    }
    serde_json::from_str(line.trim()).context("parsing daemon response")
}

// ===== PID file =====

/// Write the current process id to `path`.
pub async fn write_pid_file(path: &Path) -> Result<()> {
    tokio::fs::write(path, std::process::id().to_string())
        .await
        .with_context(|| format!("writing pid file {}", path.display()))
}

/// Read a process id from a PID file, if it exists and is well-formed.
pub async fn read_pid_file(path: &Path) -> Option<u32> {
    let raw = tokio::fs::read_to_string(path).await.ok()?;
    raw.trim().parse().ok()
}

/// Remove a PID file, ignoring "not found".
pub async fn remove_pid_file(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip_as_json() {
        for cmd in [
            DaemonCommand::Stop,
            DaemonCommand::Reload { env: Vec::new() },
            DaemonCommand::Status,
            DaemonCommand::Route {
                model: "gpt-5".to_string(),
            },
            DaemonCommand::ObserveStatus,
        ] {
            let json = serde_json::to_string(&cmd).unwrap();
            let back: DaemonCommand = serde_json::from_str(&json).unwrap();
            // tag-based round trip
            assert_eq!(std::mem::discriminant(&cmd), std::mem::discriminant(&back));
        }
    }

    #[test]
    fn observe_status_payload_round_trips() {
        let payload = ObserveStatusPayload {
            compiled_in: true,
            exporter_wired: true,
            endpoint: Some("http://collector:4318".to_string()),
            header_count: 2,
            service_name: Some("bitrouter".to_string()),
            resource_attribute_count: 1,
            sampler: Some("parentbased_always_on".to_string()),
            sampler_arg: None,
            metrics_enabled: true,
            api_key_count: 42,
            api_key_cap: 1024,
            user_id_count: 7,
            user_id_cap: 256,
            active_spans: 3,
        };
        let json = serde_json::to_string(&DaemonResponse::ObserveStatus {
            payload: payload.clone(),
        })
        .unwrap();
        let back: DaemonResponse = serde_json::from_str(&json).unwrap();
        match back {
            DaemonResponse::ObserveStatus { payload: p } => {
                assert_eq!(p.endpoint, payload.endpoint);
                assert_eq!(p.api_key_count, 42);
                assert_eq!(p.active_spans, 3);
            }
            other => panic!("expected ObserveStatus, got {other:?}"),
        }
    }

    #[test]
    fn noop_observe_status_reports_unwired() {
        let p = NoopObserveStatus { compiled_in: true }.status();
        assert!(p.compiled_in);
        assert!(!p.exporter_wired);
        assert!(p.endpoint.is_none());
        assert_eq!(p.api_key_count, 0);
    }

    #[test]
    fn legacy_unit_reload_command_still_deserialises() {
        // Pre-env wire format: `{"cmd":"reload"}` with no env field.
        // The `#[serde(default)]` on `env` keeps this working so an
        // older client (or a script speaking the v1 wire format) can
        // still issue a no-env reload.
        let back: DaemonCommand = serde_json::from_str(r#"{"cmd":"reload"}"#).unwrap();
        match back {
            DaemonCommand::Reload { env } => assert!(env.is_empty()),
            other => panic!("expected Reload, got {other:?}"),
        }
    }

    #[test]
    fn reload_command_carries_env_overrides() {
        let cmd = DaemonCommand::Reload {
            env: vec![("OPENAI_API_KEY".to_string(), "sk-test".to_string())],
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: DaemonCommand = serde_json::from_str(&json).unwrap();
        match back {
            DaemonCommand::Reload { env } => {
                assert_eq!(env.len(), 1);
                assert_eq!(
                    env[0],
                    ("OPENAI_API_KEY".to_string(), "sk-test".to_string())
                );
            }
            other => panic!("expected Reload, got {other:?}"),
        }
    }

    #[test]
    fn responses_round_trip_as_json() {
        let resp = DaemonResponse::Status {
            pid: 42,
            listen: "0.0.0.0:4356".to_string(),
            models: 3,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: DaemonResponse = serde_json::from_str(&json).unwrap();
        match back {
            DaemonResponse::Status { pid, models, .. } => {
                assert_eq!(pid, 42);
                assert_eq!(models, 3);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn resolve_socket_joins_relative_paths_onto_config_directory() {
        let resolved = resolve_socket_path(
            Path::new("/Users/x/.bitrouter/bitrouter.yaml"),
            "./bitrouter.sock",
        );
        // Leading `./` is stripped so the rendered path doesn't read
        // `…/.bitrouter/./bitrouter.sock`.
        assert_eq!(
            resolved,
            PathBuf::from("/Users/x/.bitrouter/bitrouter.sock")
        );
    }

    #[test]
    fn resolve_socket_handles_relative_without_dot_prefix() {
        let resolved = resolve_socket_path(
            Path::new("/Users/x/.bitrouter/bitrouter.yaml"),
            "bitrouter.sock",
        );
        assert_eq!(
            resolved,
            PathBuf::from("/Users/x/.bitrouter/bitrouter.sock")
        );
    }

    #[test]
    fn resolve_socket_joins_nested_relative_path() {
        let resolved = resolve_socket_path(
            Path::new("/Users/x/.bitrouter/bitrouter.yaml"),
            "sub/bitrouter.sock",
        );
        assert_eq!(
            resolved,
            PathBuf::from("/Users/x/.bitrouter/sub/bitrouter.sock")
        );
    }

    #[test]
    fn resolve_socket_passes_absolute_paths_through() {
        let resolved = resolve_socket_path(
            Path::new("/Users/x/.bitrouter/bitrouter.yaml"),
            "/var/run/bitrouter.sock",
        );
        assert_eq!(resolved, PathBuf::from("/var/run/bitrouter.sock"));
    }

    #[test]
    fn resolve_socket_handles_config_paths_without_parent() {
        // `bitrouter.yaml` with no directory component → fall back to the
        // raw socket value (CWD-relative). The leading `./` is stripped.
        let resolved = resolve_socket_path(Path::new("bitrouter.yaml"), "./bitrouter.sock");
        assert_eq!(resolved, PathBuf::from("bitrouter.sock"));
    }

    #[test]
    fn is_not_reachable_detects_socket_not_found() {
        let io_err = std::io::Error::from(std::io::ErrorKind::NotFound);
        let err: anyhow::Error =
            anyhow::Error::new(io_err).context("connecting to /tmp/bitrouter.sock");
        assert!(is_not_reachable(&err));
    }

    #[test]
    fn is_not_reachable_detects_connection_refused() {
        let io_err = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let err: anyhow::Error =
            anyhow::Error::new(io_err).context("connecting to /tmp/bitrouter.sock");
        assert!(is_not_reachable(&err));
    }

    #[test]
    fn is_not_reachable_ignores_unrelated_io_errors() {
        let io_err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let err: anyhow::Error =
            anyhow::Error::new(io_err).context("connecting to /tmp/bitrouter.sock");
        assert!(!is_not_reachable(&err));
    }

    #[test]
    fn is_not_reachable_false_for_plain_messages() {
        let err = anyhow::anyhow!("daemon closed the connection without responding");
        assert!(!is_not_reachable(&err));
    }
}
