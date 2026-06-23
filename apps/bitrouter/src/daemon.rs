//! Daemon control over a local IPC channel.
//!
//! A running `bitrouter serve` listens on a control endpoint alongside the
//! HTTP API. The CLI's `stop` / `restart` / `reload` / `status` / `route`
//! subcommands are thin clients that connect, send one newline-delimited JSON
//! [`DaemonCommand`], and read one [`DaemonResponse`].
//!
//! The transport is platform-specific but the wire protocol is identical:
//! a **Unix domain socket** (mode `0600`) on Unix, and a **Windows named
//! pipe** (`\\.\pipe\bitrouter-…`, secured by its default owner-only DACL)
//! on Windows. Both are encapsulated by a private `transport` module so the
//! rest of this file — and every caller — is platform-agnostic.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

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

/// True when `err` came from connecting to a control endpoint that nothing
/// is listening on — i.e. the daemon is stopped, not that something
/// else went wrong. Inspects the chain for an io error of kind
/// `NotFound` (socket file / pipe absent) or `ConnectionRefused` (endpoint
/// present but no acceptor, e.g. stale after a crash). On Windows a missing
/// named pipe surfaces as `ERROR_FILE_NOT_FOUND`, which maps to `NotFound`.
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

/// The daemon's self-reported readiness snapshot, returned by
/// [`probe_status`]. A successful probe means the control socket is bound
/// and the app is assembled (the model count is populated), which is exactly
/// the signal `bitrouter status` reports.
#[derive(Debug, Clone)]
pub struct ReadyInfo {
    /// The daemon's process id.
    pub pid: u32,
    /// The HTTP listen address.
    pub listen: String,
    /// Count of routable models.
    pub models: usize,
}

/// Resolve the control-socket path for a `(source, cfg)` pair. For a `File`
/// source the configured `server.control_socket` is resolved against the
/// config file's directory; for a `Default` source the socket lives at
/// `<home>/bitrouter.sock`. Single source of truth shared by `serve`,
/// `start`, and `spawn`.
pub fn socket_path_for(
    source: &crate::paths::ConfigSource,
    cfg: &bitrouter_sdk::config::Config,
) -> PathBuf {
    match source {
        crate::paths::ConfigSource::File(path) => {
            resolve_socket_path(path, &cfg.server.control_socket)
        }
        crate::paths::ConfigSource::Default { home } => home.join("bitrouter.sock"),
    }
}

/// One-shot readiness probe: send `Status` and classify the reply.
/// `Ok(Some(_))` = the daemon answered (it is up); `Ok(None)` = nothing is
/// listening yet (not reachable — keep waiting); `Err` = a daemon is there but
/// the exchange failed (a real error worth surfacing).
pub async fn probe_status(socket: &Path) -> Result<Option<ReadyInfo>> {
    match send_command(socket, &DaemonCommand::Status).await {
        Ok(DaemonResponse::Status {
            pid,
            listen,
            models,
        }) => Ok(Some(ReadyInfo {
            pid,
            listen,
            models,
        })),
        Ok(DaemonResponse::Error { message }) => Err(anyhow::anyhow!(message)),
        Ok(other) => Err(anyhow::anyhow!("unexpected response: {other:?}")),
        Err(e) if is_not_reachable(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

// ===== server side =====

/// Run the control listener until a `Stop` command is received (then it
/// returns) — run this alongside `App::serve` under a `tokio::select!`.
///
/// `listen` is the HTTP address (reported in `Status`); the endpoint is bound
/// at `socket_path` (a Unix socket file, or a named pipe derived from the path
/// on Windows) and torn down on return.
pub async fn run_control_socket(
    socket_path: PathBuf,
    app: Arc<App>,
    listen: String,
    reloader: Arc<dyn DaemonReloader>,
    observe: Arc<dyn ObserveStatusProvider>,
) -> Result<()> {
    let mut listener = transport::bind(&socket_path).await?;
    let result = accept_loop(&mut listener, &app, &listen, &reloader, &observe).await;
    listener.cleanup().await;
    result
}

async fn accept_loop(
    listener: &mut transport::ControlListener,
    app: &Arc<App>,
    listen: &str,
    reloader: &Arc<dyn DaemonReloader>,
    observe: &Arc<dyn ObserveStatusProvider>,
) -> Result<()> {
    loop {
        let stream = listener.accept().await?;
        // Handle one command per connection. A `Stop` ends the loop (and thus
        // the whole `serve`); any other command loops for the next client.
        if handle_connection(stream, app, listen, reloader, observe).await? {
            tracing::info!("stop command received — shutting down");
            return Ok(());
        }
    }
}

/// Handle one connection. Returns `Ok(true)` if it was a `Stop` command.
///
/// Generic over the concrete stream so the same logic drives a Unix
/// `UnixStream` and a Windows `NamedPipeServer` — both implement the tokio
/// async IO traits.
async fn handle_connection<S>(
    stream: S,
    app: &Arc<App>,
    listen: &str,
    reloader: &Arc<dyn DaemonReloader>,
    observe: &Arc<dyn ObserveStatusProvider>,
) -> Result<bool>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
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

async fn write_response<S>(stream: &mut S, response: &DaemonResponse) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut json = serde_json::to_string(response).context("serialising daemon response")?;
    json.push('\n');
    stream.write_all(json.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

// ===== client side =====

/// Connect to a running daemon's control endpoint and hand back the raw
/// stream. Used by [`send_command`] and by integration tests that need to
/// push bytes the normal client wouldn't (e.g. malformed input). Fails
/// clearly if no daemon is listening.
pub async fn connect_control(socket_path: &Path) -> Result<impl AsyncRead + AsyncWrite + Unpin> {
    transport::connect(socket_path).await
}

/// True when something is already bound to the control endpoint at `path`
/// (i.e. a daemon is — or was — running). On Unix this is "the socket file
/// exists"; on Windows it probes the named pipe. `restart` uses it to decide
/// whether to send a `Stop` and to wait for the old daemon to release the
/// endpoint before the replacement binds.
pub fn endpoint_in_use(path: &Path) -> bool {
    transport::endpoint_in_use(path)
}

/// Connect to a running daemon's control endpoint, send `command`, return its
/// response. Fails clearly if no daemon is listening.
pub async fn send_command(socket_path: &Path, command: &DaemonCommand) -> Result<DaemonResponse> {
    let stream = transport::connect(socket_path).await?;
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

// ===== platform transport =====

/// Unix transport: a `tokio::net::UnixListener` bound to the socket path,
/// tightened to mode `0600`. The socket file is removed on teardown.
#[cfg(unix)]
mod transport {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};
    use tokio::net::{UnixListener, UnixStream};

    /// Server-side listener wrapping a bound Unix socket.
    pub struct ControlListener {
        listener: UnixListener,
        path: PathBuf,
    }

    /// Bind the control socket, removing any stale file first and tightening
    /// permissions to owner-only.
    pub async fn bind(path: &Path) -> Result<ControlListener> {
        // A stale socket file from a crashed daemon would block the bind.
        let _ = tokio::fs::remove_file(path).await;
        let listener = UnixListener::bind(path)
            .with_context(|| format!("binding control socket {}", path.display()))?;
        // The control surface includes Stop / Reload — only the daemon owner
        // may reach it. `UnixListener::bind` respects the process umask
        // (typically 022 → 0755); tighten to 0600 explicitly so any other
        // local user is excluded.
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
        tracing::info!(socket = %path.display(), "control socket listening (mode 0600)");
        Ok(ControlListener {
            listener,
            path: path.to_path_buf(),
        })
    }

    impl ControlListener {
        /// Accept the next client connection.
        pub async fn accept(&mut self) -> Result<UnixStream> {
            let (stream, _addr) = self
                .listener
                .accept()
                .await
                .context("accepting control-socket connection")?;
            Ok(stream)
        }

        /// Remove the socket file on shutdown.
        pub async fn cleanup(self) {
            let _ = tokio::fs::remove_file(&self.path).await;
        }
    }

    /// Connect to the control socket.
    pub async fn connect(path: &Path) -> Result<UnixStream> {
        UnixStream::connect(path).await.with_context(|| {
            format!(
                "connecting to {} — is the daemon running? (`bitrouter start`)",
                path.display()
            )
        })
    }

    /// On Unix the socket is a real file: its presence means a daemon bound it.
    pub fn endpoint_in_use(path: &Path) -> bool {
        path.exists()
    }
}

/// Windows transport: a named pipe whose name is derived from the control
/// path.
///
/// Security: the pipe is created with the **default** security descriptor.
/// That grants full control to the creating user, `LocalSystem` and
/// administrators, and only `GENERIC_READ` to `Everyone` — so a co-tenant
/// non-admin user cannot perform the read+write open a control client needs
/// (it is denied with `ERROR_ACCESS_DENIED`) and therefore cannot issue
/// `Stop` / `Reload`. This is the practical analog of the Unix socket's
/// `0600` mode. Tightening to a literally owner-only DACL would require a
/// custom `SECURITY_ATTRIBUTES` pointer via
/// `ServerOptions::create_with_security_attributes_raw`, which is `unsafe` —
/// and this crate is `#![forbid(unsafe_code)]`, so we rely on the default
/// descriptor instead.
#[cfg(windows)]
mod transport {
    use std::path::Path;
    use std::time::Duration;

    use anyhow::{Context, Result};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    /// `ERROR_PIPE_BUSY` — every pipe instance is currently connected. The
    /// server re-arms a fresh instance immediately after each accept, so this
    /// is a transient race the client retries through.
    const ERROR_PIPE_BUSY: i32 = 231;

    /// Map a filesystem-style control path to a named-pipe name. An explicit
    /// `\\.\pipe\…` path is honoured verbatim; any other path is hashed into a
    /// stable, valid pipe name so the daemon and every client agree on the
    /// same endpoint regardless of the launcher's working directory. Windows
    /// paths are case-insensitive, so the name is case-folded before hashing.
    pub fn pipe_name(path: &Path) -> String {
        let raw = path.to_string_lossy();
        let lower = raw.to_ascii_lowercase();
        if lower.starts_with(r"\\.\pipe\") || lower.starts_with(r"\\?\pipe\") {
            return raw.into_owned();
        }
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(lower.as_bytes());
        let digest = hasher.finalize();
        format!(r"\\.\pipe\bitrouter-{}", hex::encode(&digest[..16]))
    }

    /// Server-side listener wrapping the currently-waiting pipe instance.
    pub struct ControlListener {
        name: String,
        /// The next instance waiting for a client. `accept` hands this out and
        /// re-arms a replacement so there is no window with no listener.
        server: NamedPipeServer,
    }

    /// Create the first pipe instance. `first_pipe_instance(true)` makes this
    /// fail loudly if another process already owns the name (e.g. a second
    /// daemon racing the same control path) rather than silently sharing it.
    pub async fn bind(path: &Path) -> Result<ControlListener> {
        let name = pipe_name(path);
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .with_context(|| format!("creating control pipe {name}"))?;
        tracing::info!(pipe = %name, "control pipe listening");
        Ok(ControlListener { name, server })
    }

    impl ControlListener {
        /// Wait for the next client, then re-arm a fresh instance and return
        /// the connected one. Mirrors a `UnixListener::accept`.
        pub async fn accept(&mut self) -> Result<NamedPipeServer> {
            self.server
                .connect()
                .await
                .context("waiting for control-pipe client")?;
            let next = ServerOptions::new()
                .create(&self.name)
                .with_context(|| format!("re-arming control pipe {}", self.name))?;
            Ok(std::mem::replace(&mut self.server, next))
        }

        /// Nothing to unlink — a named pipe disappears when its last instance
        /// handle closes.
        pub async fn cleanup(self) {}
    }

    /// Connect to the control pipe, retrying briefly through the `ERROR_PIPE_BUSY`
    /// window the server opens while re-arming between clients.
    pub async fn connect(path: &Path) -> Result<NamedPipeClient> {
        let name = pipe_name(path);
        let mut last_busy: Option<std::io::Error> = None;
        for _ in 0..50 {
            match ClientOptions::new().open(&name) {
                Ok(client) => return Ok(client),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    last_busy = Some(e);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("connecting to {name} — is the daemon running? (`bitrouter start`)")
                    });
                }
            }
        }
        let e = last_busy.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "control pipe busy")
        });
        Err(e).with_context(|| format!("connecting to {name} — daemon stayed busy"))
    }

    /// Probe whether a daemon currently owns the pipe. A successful open (or a
    /// busy reply) means it does; "not found" means it has been released.
    pub fn endpoint_in_use(path: &Path) -> bool {
        let name = pipe_name(path);
        match ClientOptions::new().open(&name) {
            Ok(_client) => true,
            Err(e) => e.raw_os_error() == Some(ERROR_PIPE_BUSY),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::pipe_name;
        use std::path::PathBuf;

        #[test]
        fn explicit_pipe_path_is_returned_verbatim() {
            // `\\.\pipe\…` is already a valid pipe name — pass it through so an
            // operator can override the derived name in `server.control_socket`.
            let p = PathBuf::from(r"\\.\pipe\bitrouter-custom");
            assert_eq!(pipe_name(&p), r"\\.\pipe\bitrouter-custom");
        }

        #[test]
        fn nt_style_pipe_path_is_returned_verbatim() {
            let p = PathBuf::from(r"\\?\pipe\bitrouter-custom");
            assert_eq!(pipe_name(&p), r"\\?\pipe\bitrouter-custom");
        }

        #[test]
        fn filesystem_path_maps_to_deterministic_pipe_name() {
            // The hash must be stable so the daemon (which binds) and the
            // client (which connects from a possibly-different CWD) agree.
            let a = pipe_name(&PathBuf::from(r"C:\Users\alice\.bitrouter\bitrouter.sock"));
            let b = pipe_name(&PathBuf::from(r"C:\Users\alice\.bitrouter\bitrouter.sock"));
            assert_eq!(a, b);
            assert!(
                a.starts_with(r"\\.\pipe\bitrouter-"),
                "unexpected pipe name {a}"
            );
        }

        #[test]
        fn case_differences_collapse_to_one_name() {
            // Windows paths are case-insensitive. If two invocations capitalise
            // the drive letter differently, they must still find the same pipe.
            let lower = pipe_name(&PathBuf::from(r"c:\users\alice\.bitrouter\bitrouter.sock"));
            let upper = pipe_name(&PathBuf::from(r"C:\Users\Alice\.BitRouter\bitrouter.sock"));
            assert_eq!(lower, upper);
        }

        #[test]
        fn different_paths_map_to_different_pipe_names() {
            let a = pipe_name(&PathBuf::from(r"C:\Users\alice\.bitrouter\bitrouter.sock"));
            let b = pipe_name(&PathBuf::from(r"C:\Users\bob\.bitrouter\bitrouter.sock"));
            assert_ne!(a, b);
        }
    }
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

// ===== launch + readiness wait =====

/// How long [`start_and_wait`] polls for readiness before giving up. Sized for
/// a cold registry fetch + DB migrations on first run; the daemon keeps running
/// past this — the timeout only bounds how long the launcher blocks.
pub const DAEMON_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Poll cadence for the readiness loop.
const READY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// The result of launching a detached daemon and waiting for it to come up.
#[derive(Debug)]
pub enum DaemonStartOutcome {
    /// The daemon answered `Status` within the timeout. Carries its self-report.
    Ready(ReadyInfo),
    /// The process is alive but did not become ready before the deadline.
    NotReadyInTime {
        /// The launched child's process id.
        pid: u32,
    },
    /// The process exited during startup. Carries the exit-status string and
    /// the fresh tail of the daemon log (this run's output only).
    Exited {
        /// Rendered `ExitStatus`.
        status: String,
        /// Daemon log content written since launch.
        log_tail: String,
    },
}

/// Spawn `bitrouter serve` as a detached background process writing to
/// `log_path` (append). Returns the child handle and the log's pre-spawn byte
/// length so the caller can quote only this run's output on early death.
///
/// Detach rationale: a new process group on Unix so the launcher shell's
/// SIGHUP (terminal close) does not propagate; DETACHED_PROCESS +
/// CREATE_NEW_PROCESS_GROUP on Windows so a console Ctrl-C is not delivered.
/// <https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html#tymethod.process_group>
fn spawn_detached_serve(
    source: &crate::paths::ConfigSource,
    log_path: &Path,
) -> Result<(std::process::Child, u64)> {
    let exe = std::env::current_exe().context("locating current bitrouter binary")?;
    // Capture the log's pre-spawn size so we can quote *this run's* output back
    // to the user on early death instead of slurping stale content from prior
    // runs (the log is opened append-only).
    let log_size_before = std::fs::metadata(log_path).map(|m| m.len()).unwrap_or(0);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening daemon log {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .context("duplicating log handle for stderr")?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("serve");
    // For a `File` source pass `--config <abs path>` so the child loads the same
    // file even though it'll chdir to the home. For `Default` (zero-config) skip
    // the flag — the child re-runs `resolve_config` and lands on the same state.
    if let crate::paths::ConfigSource::File(path) = source {
        cmd.arg("--config").arg(path);
    }
    cmd.stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err))
        .stdin(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Winbase.h constants — avoid pulling in a `windows`/`winapi` crate.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let child = cmd.spawn().context("spawning detached `bitrouter serve`")?;
    Ok((child, log_size_before))
}

/// Read the daemon log from `offset` to end. Returns "" on any read failure so
/// callers can fall back to a path-only hint. The pre-spawn `offset` ensures
/// only this run's content is quoted even though the log is append-only.
async fn read_log_tail(path: &Path, offset: u64) -> String {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    let start = (offset as usize).min(bytes.len());
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Print the daemon's tail-of-log to stderr as an indented, captioned block so
/// the user sees the actual failure inline instead of being pointed at a log
/// file they have to open separately. No-op when there is nothing useful to
/// show. Shared by `start` and `spawn`.
pub fn eprint_failure_log(log_path: &Path, content: &str) {
    let trimmed = content.trim_end();
    if trimmed.is_empty() {
        return;
    }
    let p = crate::style::Palette::for_stderr();
    eprintln!(
        "{dim}daemon log ({path}):{reset}",
        dim = p.dim,
        path = log_path.display(),
        reset = p.reset,
    );
    for line in trimmed.lines() {
        eprintln!("  {line}");
    }
    eprintln!();
}

/// Launch a detached `bitrouter serve` and poll the control socket until it
/// answers `Status`, the process dies, or `timeout` elapses. The daemon keeps
/// running regardless of the outcome — this only reports what the launcher
/// observed. `socket` is the control-socket path to poll; pass `None` when it
/// could not be resolved (then only process-death is detectable).
///
/// Ensures the bitrouter home exists (the log lives inside it) but never chdirs
/// the calling process — only the child `serve` chdirs into the home.
pub async fn start_and_wait(
    source: &crate::paths::ConfigSource,
    log_path: &Path,
    socket: Option<&Path>,
    timeout: std::time::Duration,
) -> Result<DaemonStartOutcome> {
    crate::paths::ensure_home_directory(source.home())?;
    let (mut child, log_size_before) = spawn_detached_serve(source, log_path)?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        // Early-death: a bad config / port-in-use kills the child fast; surface
        // it with the log tail rather than waiting out the whole timeout. Any
        // exit before readiness means the daemon isn't serving, so the status
        // code is reported verbatim and treated as a startup failure by callers
        // (`serve` never exits 0 before it binds the control socket).
        if let Ok(Some(status)) = child.try_wait() {
            let log_tail = read_log_tail(log_path, log_size_before).await;
            return Ok(DaemonStartOutcome::Exited {
                status: status.to_string(),
                log_tail,
            });
        }
        if let Some(socket) = socket {
            match probe_status(socket).await {
                Ok(Some(info)) => return Ok(DaemonStartOutcome::Ready(info)),
                Ok(None) => {}
                Err(e) => tracing::debug!(error = %e, "readiness probe failed; retrying"),
            }
        }
        if std::time::Instant::now() >= deadline {
            return Ok(DaemonStartOutcome::NotReadyInTime { pid: child.id() });
        }
        tokio::time::sleep(READY_POLL_INTERVAL).await;
    }
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
