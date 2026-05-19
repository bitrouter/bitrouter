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
    /// Hot-reload the config / routing table.
    Reload,
    /// Report daemon status.
    Status,
    /// Resolve a model name through the live routing table.
    Route {
        /// The model name to resolve.
        model: String,
    },
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
    /// The command failed.
    Error {
        /// Human-readable failure detail.
        message: String,
    },
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

    let result = accept_loop(&listener, &app, &listen, &reloader).await;
    let _ = tokio::fs::remove_file(&socket_path).await;
    result
}

async fn accept_loop(
    listener: &UnixListener,
    app: &Arc<App>,
    listen: &str,
    reloader: &Arc<dyn DaemonReloader>,
) -> Result<()> {
    loop {
        let (stream, _addr) = listener
            .accept()
            .await
            .context("accepting control-socket connection")?;
        // Handle one command per connection. A `Stop` ends the loop (and thus
        // the whole `serve`); any other command loops for the next client.
        if handle_connection(stream, app, listen, reloader).await? {
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
    let response = dispatch(command, app, listen, reloader).await;
    write_response(reader.get_mut(), &response).await?;
    Ok(is_stop)
}

async fn dispatch(
    command: DaemonCommand,
    app: &Arc<App>,
    listen: &str,
    reloader: &Arc<dyn DaemonReloader>,
) -> DaemonResponse {
    match command {
        DaemonCommand::Stop => DaemonResponse::Ok,
        DaemonCommand::Reload => match reloader.reload().await {
            Ok(()) => {
                tracing::info!("reload succeeded");
                DaemonResponse::Ok
            }
            Err(e) => DaemonResponse::Error {
                message: format!("reload failed: {e}"),
            },
        },
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
            DaemonCommand::Reload,
            DaemonCommand::Status,
            DaemonCommand::Route {
                model: "gpt-5".to_string(),
            },
        ] {
            let json = serde_json::to_string(&cmd).unwrap();
            let back: DaemonCommand = serde_json::from_str(&json).unwrap();
            // tag-based round trip
            assert_eq!(std::mem::discriminant(&cmd), std::mem::discriminant(&back));
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
