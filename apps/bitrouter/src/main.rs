//! `bitrouter` CLI entry point — a thin shell over the `bitrouter` lib.
//!
//! Subcommand surface: `serve` / `start` / `stop` / `restart` /
//! `reload` / `status` / `route` / `init` / `key sign` / `models` / `tools` /
//! `policy create` / `providers (list|use)` / `wallet` / `login` / `logout` /
//! `whoami` / `agents`. Daemon control runs over a Unix socket —
//! `start` spawns `serve` detached; the client subcommands send one
//! [`DaemonCommand`] each.
//!
//! v1.0 ships the routing / settlement subsystems wired here. Subsystems that
//! belong to *other* services (OWS wallet, cloud login, ACP runtime) print an
//! honest "not implemented in v1.0" message rather than faking output — see
//! 007's notes on cross-system scope.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use bitrouter::commands;
use bitrouter::daemon::{self, DaemonCommand, DaemonResponse, RouteHop};
use bitrouter_sdk::config;

/// BitRouter — an LLM API router.
#[derive(Parser)]
#[command(name = "bitrouter", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Load a config, run migrations, and serve HTTP + control socket
    /// **in the foreground**.
    Serve {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Spawn `bitrouter serve` as a detached background process.
    Start {
        /// Path to `bitrouter.yaml` (passed through to the child).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Path to redirect the daemon's stdout/stderr to. Defaults to
        /// `bitrouter.log` inside the config file's directory (e.g.
        /// `~/.bitrouter/bitrouter.log`) so it lives alongside the
        /// socket and pid file rather than in the launcher's CWD.
        #[arg(long)]
        log: Option<PathBuf>,
    },
    /// Send a `stop` command to a running daemon.
    Stop {
        /// Path to `bitrouter.yaml` (used to locate the control socket).
        /// Resolves via the standard chain: `./bitrouter.yaml` →
        /// `$BITROUTER_HOME/bitrouter.yaml` → `~/.bitrouter/bitrouter.yaml`.
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// `stop` then `start` — config path is passed through.
    Restart {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Path to redirect the new daemon's stdout/stderr to. Defaults
        /// to `bitrouter.log` next to the config file.
        #[arg(long)]
        log: Option<PathBuf>,
    },
    /// Hot-reload the running daemon's config / routing table.
    Reload {
        /// Path to `bitrouter.yaml` (used to locate the control socket).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Report a running daemon's status (pid, listen address, model count).
    /// Prints `running: no` when no daemon is reachable.
    Status {
        /// Path to `bitrouter.yaml` (used to locate the control socket).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Resolve a model name through the routing table. Uses the running
    /// daemon if reachable, otherwise loads the config and resolves locally.
    Route {
        /// The model name to resolve.
        model: String,
        /// Path to `bitrouter.yaml` (used as the standalone fallback and
        /// to locate the control socket).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Write a starter `bitrouter.yaml` (with `skip_auth: true`).
    Init {
        /// Path to write.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
    },
    /// Virtual-key management.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// List routable models for a config, optionally filtered by provider.
    Models {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Show only models declared by this provider.
        #[arg(short, long)]
        provider: Option<String>,
    },
    /// MCP server introspection — list/status/discover against the upstreams
    /// declared under `mcp_servers` in `bitrouter.yaml`. v1.0 does not maintain
    /// a global tool registry; these are one-shot queries.
    Tools {
        #[command(subcommand)]
        action: ToolsAction,
    },
    /// Observability inspection (OTel exporter state, cardinality usage).
    Observe {
        #[command(subcommand)]
        action: ObserveAction,
    },
    /// Policy management.
    Policy {
        #[command(subcommand)]
        action: PolicyAction,
    },
    /// Provider management.
    Providers {
        #[command(subcommand)]
        action: ProviderAction,
    },
    /// OWS wallet integration — not implemented in v1.0.
    Wallet,
    /// Log in to an upstream provider — interactive credential setup.
    ///
    /// Per-provider available methods are auto-derived from the catalog:
    /// `anthropic` prompts for **subscription** (Claude Pro/Max browser
    /// PKCE) **or** **API key** paste; `openai-codex` runs the ChatGPT
    /// subscription PKCE flow; `github-copilot` runs the GitHub device
    /// code flow; everything else accepts a pasted API key. The
    /// resulting credential is stored under
    /// `$XDG_DATA_HOME/bitrouter/oauth-tokens.json` keyed by
    /// `(provider_id, label)`. For cloud sign-in (no argument), see
    /// `bitrouter auth login` instead — kept separate so the per-
    /// provider and cloud flows don't share a flag surface.
    Login {
        /// Provider id to log in to (e.g. `anthropic`, `openai-codex`,
        /// `github-copilot`). Omit and the CLI redirects you to
        /// `bitrouter auth login` for the cloud flow.
        provider: Option<String>,
        /// Account label this credential is stored under. Defaults to
        /// `default`. Use a non-default label to keep multiple accounts
        /// of the same provider side by side — reference them from
        /// `accounts:` entries in `bitrouter.yaml`.
        #[arg(short, long, default_value = "default")]
        label: String,
    },
    /// Log out of an upstream provider — clears every stored credential
    /// for the provider (subscription OAuth and pasted API keys alike).
    /// For cloud sign-out (no argument), see `bitrouter auth logout`.
    Logout {
        /// Provider id whose stored credentials should be removed.
        /// Omit and the CLI redirects you to `bitrouter auth logout`.
        provider: Option<String>,
    },
    /// Legacy shim. Cloud identity now lives under
    /// `bitrouter auth whoami` (local) and `bitrouter cloud whoami`
    /// (local + base URL); this prints a pointer to those.
    Whoami,
    /// ACP agent lifecycle — list the catalog, check configured agents,
    /// print install stubs. `bitrouter agent-proxy <id>` is the separate
    /// stdio bridge an editor spawns.
    Agents {
        #[command(subcommand)]
        action: AgentsAction,
    },
    /// Stdio bridge between an ACP-aware editor and a configured upstream
    /// agent. Routes inbound JSON-RPC requests through the `acp` pipeline,
    /// relays upstream notifications back to the editor.
    #[command(name = "agent-proxy")]
    AgentProxy {
        /// Agent id (must exist under `agents:` in the config).
        agent: String,
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Sign in to bitrouter from this terminal.
    ///
    /// After `bitrouter auth login`, this CLI uses your account credentials
    /// automatically for inference, key management, billing, BYOK, and the
    /// rest of the management surface.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Manage your BitRouter Cloud account — API keys, usage, billing,
    /// policies, BYOK, OAuth clients. Requires `bitrouter auth login`
    /// first.
    Cloud {
        #[command(subcommand)]
        action: bitrouter::cloud::cli::CloudAction,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Sign in to bitrouter.
    ///
    /// Prints a verification URL — open it in your browser, approve, and
    /// this CLI receives an access token it stores locally and refreshes
    /// automatically.
    Login {
        /// Authorization server URL. Defaults to https://api.bitrouter.ai;
        /// override only for a self-hosted deployment (env: BITROUTER_OAUTH_AS).
        #[arg(long = "oauth-as", value_name = "URL")]
        authorization_server: Option<String>,
        /// OAuth client id. Defaults to `bitrouter-cli`; override only for a
        /// self-hosted deployment (env: BITROUTER_OAUTH_CLIENT_ID).
        #[arg(long = "client-id", value_name = "ID")]
        client_id: Option<String>,
        /// Permissions to request, as a space-delimited list. Defaults to a
        /// broad "developer" set (inference, key management, billing-read,
        /// policy, BYOK, account-read); pass a narrower or wider list to
        /// override (env: BITROUTER_OAUTH_SCOPE).
        #[arg(long, value_name = "SCOPE")]
        scope: Option<String>,
    },
    /// Sign out: revoke the stored token at the server (best-effort) and
    /// delete the local credentials file.
    Logout {
        /// Override the authorization server URL recorded in the
        /// credentials file for the revocation call.
        #[arg(long = "oauth-as", value_name = "URL")]
        authorization_server: Option<String>,
        /// Override the recorded OAuth client id for the revocation call.
        #[arg(long = "client-id", value_name = "ID")]
        client_id: Option<String>,
    },
    /// Show who is signed in on this machine.
    ///
    /// Reads the locally stored credentials — no network call.
    Whoami,
}

#[derive(Subcommand)]
enum AgentsAction {
    /// Show the bundled v1.0 catalog of well-known agents and which of
    /// them are present under `agents:` in the loaded config.
    List {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Spawn each configured agent and verify it answers `initialize`.
    Check {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Print a YAML stub for an agent in the catalog (paste under
    /// `agents:` in `bitrouter.yaml`).
    Install {
        /// Agent id from the catalog (see `bitrouter agents list`).
        id: String,
    },
}

#[derive(Subcommand)]
enum ObserveAction {
    /// Report the OTel exporter's current state (endpoint, sampler,
    /// cardinality usage, in-flight spans). Queries the running daemon
    /// over the control socket; reports "stopped" + the compile-time
    /// `OTEL_ENABLED` flag when no daemon is reachable.
    Status {
        /// Path to `bitrouter.yaml` (used to locate the control socket).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Explicit control socket path. Overrides the config-derived path.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Emit the snapshot as JSON instead of the human-readable block.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ToolsAction {
    /// List tools advertised by every configured MCP server.
    List {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Health-check every configured MCP server with a `tools/list` round-trip.
    Status {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Connect to one MCP server and print a YAML stub suitable for pasting
    /// into `mcp_servers:`.
    Discover {
        /// Server id (must exist under `mcp_servers` in the config).
        server: String,
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum KeyAction {
    /// Mint a new `brvk_` virtual key for a user. v1 does not sign a JWT — it
    /// creates a DB-backed virtual key and prints the plaintext once.
    Sign {
        /// The owning user id.
        #[arg(short, long)]
        user: String,
        /// Database URL — any backend sea-orm supports
        /// (`sqlite://…`, `postgres://…`, `mysql://…`).
        #[arg(short, long, default_value = "sqlite://./bitrouter.db")]
        db: String,
        /// Optional policy id to bind to the key (the `policy_id` column).
        #[arg(long)]
        policy: Option<String>,
    },
}

#[derive(Subcommand)]
enum PolicyAction {
    /// Write a starter policy file to the policy dir.
    Create {
        /// Policy id (becomes the file stem and the `id:` field).
        id: String,
        /// Policy directory. Default matches the assembly default.
        #[arg(long, default_value = "./policies")]
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum ProviderAction {
    /// List every configured provider.
    List {
        /// Path to `bitrouter.yaml`. When omitted, the binary resolves
        /// in this order: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml`
        /// → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory defaults
        /// (`bitrouter init` is the explicit way to scaffold a file).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Select an active provider. v1 has no "current provider" concept (003
    /// §5.2 dropped the v0 DEFAULT_PROVIDER fallback) — this command is a
    /// no-op kept for surface compatibility with v0's `providers use`.
    Use {
        /// Provider id (accepted but not persisted).
        id: String,
    },
}

#[tokio::main]
async fn main() {
    // Defer to `run` for the actual dispatch so the entry point can
    // route the resulting `Result` through `error_report::report` instead
    // of leaking anyhow's verbose `Debug` formatter to end users.
    match run().await {
        Ok(()) => {}
        Err(e) => {
            bitrouter::error_report::report(&e);
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            serve(&source).await
        }
        Command::Start { config, log } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let log_path = resolve_log_path(source.home(), log.as_deref());
            start(&source, &log_path).await
        }
        Command::Stop { config, socket } => {
            let socket = resolve_client_socket(config.as_deref(), socket.as_deref()).await?;
            stop(&socket).await
        }
        Command::Restart {
            config,
            socket,
            log,
        } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let socket = resolve_client_socket_from(&source, socket.as_deref()).await?;
            let log_path = resolve_log_path(source.home(), log.as_deref());
            restart(&source, &socket, &log_path).await
        }
        Command::Reload { config, socket } => {
            let socket = resolve_client_socket(config.as_deref(), socket.as_deref()).await?;
            reload(&socket).await
        }
        Command::Status { config, socket } => {
            let socket = resolve_client_socket(config.as_deref(), socket.as_deref()).await?;
            status(&socket).await
        }
        Command::Route {
            model,
            config,
            socket,
        } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let socket = resolve_client_socket_from(&source, socket.as_deref()).await?;
            route(&model, &source, &socket).await
        }
        Command::Init { config } => init(&config).await,
        Command::Key { action } => key(action).await,
        Command::Models { config, provider } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            models(&source, provider.as_deref()).await
        }
        Command::Tools { action } => tools(action).await,
        Command::Observe { action } => observe(action).await,
        Command::Policy { action } => policy(action).await,
        Command::Providers { action } => providers(action).await,
        Command::Wallet => {
            print_unimplemented(
                "wallet",
                "OWS wallet integration is delivered by the `ows` workspace, not\n\
                 by bitrouter. v1.0 ships the routing / settlement layer only.",
            );
            Ok(())
        }
        Command::Login { provider, label } => match provider.as_deref() {
            Some(name) => bitrouter::commands::login_provider(name, &label).await,
            None => {
                print_unimplemented(
                    "login",
                    "`bitrouter login` is the per-provider OAuth surface.\n\
                     For cloud sign-in, run `bitrouter auth login`.\n\
                     For a local virtual key, run `bitrouter key sign --user <id>`.",
                );
                Ok(())
            }
        },
        Command::Logout { provider } => match provider.as_deref() {
            Some(name) => bitrouter::commands::logout_provider(name).await,
            None => {
                print_unimplemented(
                    "logout",
                    "`bitrouter logout` is the per-provider OAuth surface.\n\
                     For cloud sign-out, run `bitrouter auth logout`.",
                );
                Ok(())
            }
        },
        Command::Whoami => {
            print_unimplemented(
                "whoami",
                "`bitrouter whoami` (top-level) is a legacy shim. Use:\n\
                 - `bitrouter auth whoami`  — local cloud identity (offline read).\n\
                 - `bitrouter cloud whoami` — same identity plus the configured /v1/* base URL.",
            );
            Ok(())
        }
        Command::Agents { action } => agents_cmd(action).await,
        Command::AgentProxy { agent, config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            agent_proxy_cmd(&agent, &source).await
        }
        Command::Auth { action } => auth_cmd(action).await,
        Command::Cloud { action } => bitrouter::cloud::cli::run(action).await,
    }
}

// ===== `bitrouter auth …` (OAuth 2.0 device flow against a user-supplied AS) =====

async fn auth_cmd(action: AuthAction) -> Result<()> {
    use bitrouter_cloud_sdk::auth::commands::{LoginInputs, login, logout, whoami};
    match action {
        AuthAction::Login {
            authorization_server,
            client_id,
            scope,
        } => {
            login(LoginInputs {
                authorization_server,
                client_id,
                scope,
            })
            .await?;
            Ok(())
        }
        AuthAction::Logout {
            authorization_server,
            client_id,
        } => {
            logout(LoginInputs {
                authorization_server,
                client_id,
                scope: None,
            })
            .await
        }
        AuthAction::Whoami => whoami().await,
    }
}

// ===== serve / daemon control =====

/// Resolve the control-socket path for a *daemon-control* subcommand
/// (`stop`, `reload`, `status`). An explicit `--socket` override wins;
/// otherwise we resolve the config path via the standard chain, try to
/// load the YAML to read `server.control_socket`, and join the value
/// onto the config file's directory.
///
/// Loading the YAML is **best-effort**: a broken or env-var-incomplete
/// config falls back to the default socket name in the same directory.
/// That keeps `bitrouter status` answerable in exactly the state where
/// the user most wants to ask (config can't load → daemon can't be
/// running → "stopped"). The "real" config error still surfaces the
/// next time the user runs `serve` / `start`.
async fn resolve_client_socket(config: Option<&Path>, socket: Option<&Path>) -> Result<PathBuf> {
    if let Some(s) = socket {
        return Ok(s.to_path_buf());
    }
    let source = bitrouter::paths::resolve_config(config)?;
    match &source {
        bitrouter::paths::ConfigSource::File(path) => {
            let socket_str = match config::load(path).await {
                Ok(cfg) => cfg.server.control_socket,
                Err(_) => daemon::DEFAULT_CONTROL_SOCKET.to_string(),
            };
            Ok(daemon::resolve_socket_path(path, &socket_str))
        }
        bitrouter::paths::ConfigSource::Default { home } => Ok(home.join("bitrouter.sock")),
    }
}

/// Resolve the `bitrouter.log` path for `start` / `restart`. An
/// explicit `--log` override wins; otherwise we place the log next to
/// the config file (e.g. `~/.bitrouter/bitrouter.log`) so the daemon's
/// runtime artefacts — config, socket, pid file, log — all live in one
/// directory. The legacy default of `./bitrouter.log` would land the
/// log file in whichever CWD the launcher happened to be in.
fn resolve_log_path(home: &Path, log: Option<&Path>) -> PathBuf {
    if let Some(l) = log {
        return l.to_path_buf();
    }
    home.join("bitrouter.log")
}

/// Variant of [`resolve_client_socket`] for subcommands (`restart`,
/// `route`) that load the config for other reasons anyway, so a config
/// failure is a real error worth surfacing.
async fn resolve_client_socket_from(
    source: &bitrouter::paths::ConfigSource,
    socket: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(s) = socket {
        return Ok(s.to_path_buf());
    }
    match source {
        bitrouter::paths::ConfigSource::File(path) => {
            let cfg = config::load(path)
                .await
                .with_context(|| format!("loading {}", path.display()))?;
            Ok(daemon::resolve_socket_path(
                path,
                &cfg.server.control_socket,
            ))
        }
        bitrouter::paths::ConfigSource::Default { home } => Ok(home.join("bitrouter.sock")),
    }
}

async fn serve(source: &bitrouter::paths::ConfigSource) -> Result<()> {
    // Ensure the bitrouter home directory exists (zero-config first-run
    // creates `~/.bitrouter` on demand) and chdir into it. Every
    // relative path in the config — `database.url`,
    // `server.control_socket`, policy / agent / mcp file references —
    // then interprets relative to one stable location instead of
    // whichever CWD the launcher happened to be in. The daemon's
    // runtime artefacts (db, socket, pid, log) all land in the home.
    let home = source.home();
    bitrouter::paths::ensure_home_directory(home)?;
    std::env::set_current_dir(home)
        .with_context(|| format!("chdir to bitrouter home {}", home.display()))?;

    let cfg = bitrouter::paths::load_config(source).await?;
    announce_zero_config(source, &cfg);
    let listen = cfg.server.listen.clone();
    // For a `File` source we resolve the socket against the config
    // file's directory (preserves any user override). For `Default`
    // the socket lives at `<home>/bitrouter.sock` directly.
    let socket_path = match source {
        bitrouter::paths::ConfigSource::File(path) => {
            daemon::resolve_socket_path(path, &cfg.server.control_socket)
        }
        bitrouter::paths::ConfigSource::Default { home } => home.join("bitrouter.sock"),
    };
    let pid_path = pid_path_for(&socket_path);

    let config_path_for_reload = match source {
        bitrouter::paths::ConfigSource::File(path) => Some(path.as_path()),
        bitrouter::paths::ConfigSource::Default { .. } => None,
    };
    let assembled = bitrouter::build_app_with_path(&cfg, config_path_for_reload).await?;
    let app = Arc::new(assembled.app);
    let policy_store = assembled.policy_store;
    // Clone before moving the original into `run_control_socket` — we
    // need a handle here too so the shutdown path below can drive the
    // exporter flush before the runtime tears down.
    let observe_provider = assembled.observe;
    let observe_for_shutdown = observe_provider.clone();
    let reload_source = match source {
        bitrouter::paths::ConfigSource::File(path) => {
            bitrouter::reload::ReloadSource::File(path.clone())
        }
        bitrouter::paths::ConfigSource::Default { .. } => bitrouter::reload::ReloadSource::Default,
    };
    let reloader: Arc<dyn daemon::DaemonReloader> = Arc::new(bitrouter::reload::AppReloader::new(
        policy_store.clone(),
        assembled.routing_table,
        reload_source,
    ));

    daemon::write_pid_file(&pid_path).await?;
    println!(
        "bitrouter {} — serving on {listen} (control: {})",
        bitrouter::VERSION,
        socket_path.display()
    );

    let http_app = app.clone();
    let http_listen = listen.clone();
    let http = async move {
        http_app
            .serve(&http_listen)
            .await
            .map_err(anyhow::Error::from)
    };
    let control = daemon::run_control_socket(
        socket_path,
        app.clone(),
        listen,
        reloader.clone(),
        observe_provider,
    );

    // SIGHUP triggers a config reload — per, reload should be
    // available via either `bitrouter reload` (the socket path) *or* a HUP
    // signal. Same fan-out as the Reload command — every reloadable subsystem.
    let hup_reloader = reloader.clone();
    let hup = async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut hup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => return Err::<(), _>(anyhow::Error::from(e)),
        };
        loop {
            if hup.recv().await.is_none() {
                return Ok(());
            }
            match hup_reloader.reload().await {
                Ok(()) => tracing::info!("SIGHUP — reload succeeded"),
                Err(e) => tracing::warn!(error = %e, "SIGHUP reload failed"),
            }
        }
    };

    // SIGINT (ctrl-C) and SIGTERM (systemd / `kill`) end the loop the
    // same way `bitrouter stop` does — so the shutdown path below
    // (observe flush, pid-file cleanup) runs in every termination
    // mode except SIGKILL.
    let term = async {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).map_err(anyhow::Error::from)?;
        let mut sigterm = signal(SignalKind::terminate()).map_err(anyhow::Error::from)?;
        tokio::select! {
            _ = sigint.recv() => tracing::info!("SIGINT — shutting down"),
            _ = sigterm.recv() => tracing::info!("SIGTERM — shutting down"),
        }
        Ok::<(), anyhow::Error>(())
    };

    let result = tokio::select! {
        r = http => r,
        r = control => r,
        // SIGHUP loop never returns Ok by design; an error from signal setup
        // is logged and we keep serving.
        r = hup => match r {
            Ok(()) => Ok(()),
            Err(e) => { tracing::warn!(error = %e, "SIGHUP listener unavailable"); Ok(()) }
        },
        r = term => match r {
            Ok(()) => Ok(()),
            Err(e) => { tracing::warn!(error = %e, "SIGINT/SIGTERM listener unavailable"); Ok(()) }
        },
    };

    // Drive the OTel exporter's flush before anything else drops — its
    // `rt-tokio` background tasks need a live async runtime to drain,
    // and `spawn_blocking` (inside the provider's `shutdown`) parks on
    // a dedicated thread so the runtime is free to keep ticking. The
    // impl is idempotent: a follow-up Drop is a no-op.
    observe_for_shutdown.shutdown().await;

    daemon::remove_pid_file(&pid_path).await;
    result
}

async fn start(source: &bitrouter::paths::ConfigSource, log_path: &Path) -> Result<()> {
    // Make sure the bitrouter home exists *before* we open the log
    // file inside it. (Zero-config first-run lands here with the home
    // not yet created on disk.)
    bitrouter::paths::ensure_home_directory(source.home())?;

    // Refuse to start a second daemon on top of a live one — silent overlap
    // would race two `serve`s for the same socket and one would die into the
    // log file (the user wouldn't see it).
    let cfg_socket_path: Option<PathBuf> = match source {
        bitrouter::paths::ConfigSource::File(path) => match config::load(path).await {
            Ok(cfg) => Some(daemon::resolve_socket_path(
                path,
                &cfg.server.control_socket,
            )),
            Err(_) => None,
        },
        bitrouter::paths::ConfigSource::Default { home } => Some(home.join("bitrouter.sock")),
    };
    if let Some(socket) = &cfg_socket_path {
        let pid_path = pid_path_for(socket);
        if let Some(pid) = daemon::read_pid_file(&pid_path).await {
            if process_is_alive(pid) {
                anyhow::bail!(
                    "bitrouter is already running (pid {pid}); use `restart` or `stop` first"
                );
            }
            // Stale PID file — clean up before proceeding.
            daemon::remove_pid_file(&pid_path).await;
        }
    }

    let exe = std::env::current_exe().context("locating current bitrouter binary")?;
    // Capture the log's pre-spawn size so we can quote *this run's*
    // output back to the user on early death instead of slurping
    // stale content from prior runs (the log is opened append-only).
    let log_size_before = std::fs::metadata(log_path).map(|m| m.len()).unwrap_or(0);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening daemon log {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .context("duplicating log handle for stderr")?;

    // Detach the child into its own process group so the parent shell's
    // SIGHUP (terminal close) does not propagate to it. Otherwise closing the
    // tab would kill the daemon. Pattern from
    // https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html#tymethod.process_group
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("serve");
    // For a `File` source pass `--config <abs path>` so the child
    // loads the same file even though it'll chdir to the home. For
    // `Default` (zero-config) skip the flag — the child re-runs
    // `resolve_config`, finds no file, and arrives at the same
    // zero-config state.
    if let bitrouter::paths::ConfigSource::File(path) = source {
        cmd.arg("--config").arg(path);
    }
    let mut child = cmd
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err))
        .stdin(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .context("spawning detached `bitrouter serve`")?;

    // Liveness grace period: if the child explodes immediately (bad config,
    // port already in use, …) we want the user to know now, not in the log.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    if let Ok(Some(status)) = child.try_wait() {
        let tail = read_log_since(log_path, log_size_before).await;
        eprint_failure_log(log_path, &tail);
        anyhow::bail!("daemon exited immediately ({status})");
    }

    println!(
        "bitrouter daemon started (pid {}) — logs at {}",
        child.id(),
        log_path.display()
    );
    Ok(())
}

/// Tell the operator they're running zero-config — and exactly which
/// providers auto-enabled from the environment, so the absence of a
/// model later doesn't read as a bug. No-op for a `File` source.
fn announce_zero_config(
    source: &bitrouter::paths::ConfigSource,
    cfg: &bitrouter_sdk::config::Config,
) {
    if !source.is_default() {
        return;
    }
    let enabled: Vec<&str> = cfg.providers.keys().map(String::as_str).collect();
    if enabled.is_empty() {
        print_onboarding_hint();
    } else {
        bitrouter::error_report::info(format_args!(
            "zero-config mode — auto-enabled providers: {}",
            enabled.join(", ")
        ));
    }
}

/// Multi-line guidance shown when zero-config detects no credential of any
/// kind. The recommendation chain is intentional:
///
///   1. `bitrouter auth login` — one OAuth account, every supported model.
///   2. `BITROUTER_API_KEY` — long-lived `brk_…` key, same coverage.
///   3. Any upstream provider the user already pays for, locally.
///
/// Rendered directly (not through `error_report::info`) because that helper
/// is single-line by design.
fn print_onboarding_hint() {
    let p = bitrouter::style::Palette::for_stderr();
    eprintln!(
        "{cyan}{bold}info:{reset} no providers are configured yet. Choose one:",
        cyan = p.cyan,
        bold = p.bold,
        reset = p.reset,
    );
    eprintln!();
    eprintln!("  1. Sign in to BitRouter Cloud — one account covers every model:");
    eprintln!();
    eprintln!("       bitrouter auth login");
    eprintln!("       bitrouter cloud --help        # manage keys, usage, policies, billing");
    eprintln!();
    eprintln!("  2. Or paste a BitRouter API key:");
    eprintln!();
    eprintln!("       export BITROUTER_API_KEY=brk_…");
    eprintln!();
    eprintln!("  3. Or use a provider you already pay for, locally:");
    eprintln!();
    eprintln!("       bitrouter login anthropic            # Claude Pro/Max subscription");
    eprintln!("       bitrouter login github-copilot       # GitHub Copilot subscription");
    eprintln!("       bitrouter login openai-codex         # ChatGPT subscription");
    eprintln!();
    eprintln!("     …or set an API-key env var:");
    eprintln!();
    let env_vars = other_provider_env_var_hints();
    for var in &env_vars {
        eprintln!("       export {var}=…");
    }
    eprintln!();
}

/// Deduplicated, sorted env-var names for every built-in provider except
/// `BITROUTER_API_KEY` (rendered separately as step 2). Used by the
/// onboarding hint.
fn other_provider_env_var_hints() -> Vec<String> {
    let mut vars: Vec<String> = bitrouter_providers::zero_config_env_var_providers()
        .into_iter()
        .map(|(_, env)| env.to_string())
        .filter(|v| v != "BITROUTER_API_KEY")
        .collect();
    vars.sort();
    vars.dedup();
    vars
}

/// Read the daemon log from `offset` to end. Used to recover the
/// freshly-written failure output when the spawned child dies during
/// the liveness grace period — the pre-spawn offset captured by
/// [`start`] ensures we only quote *this* run's content even though
/// the log is opened append-only and may carry history.
///
/// Returns an empty string on any read failure (missing file, permission
/// error, decode hiccup) so the caller can fall back to a path-only
/// hint without panicking on the user's worst day.
async fn read_log_since(path: &Path, offset: u64) -> String {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    let start = (offset as usize).min(bytes.len());
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Print the daemon's tail-of-log to stderr as an indented, captioned
/// block so the user sees the actual failure inline instead of being
/// pointed at a log file they have to open separately. Silent no-op
/// when there is nothing useful to show.
fn eprint_failure_log(log_path: &Path, content: &str) {
    let trimmed = content.trim_end();
    if trimmed.is_empty() {
        return;
    }
    let p = bitrouter::style::Palette::for_stderr();
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

async fn stop(socket: &Path) -> Result<()> {
    match daemon::send_command(socket, &DaemonCommand::Stop).await? {
        DaemonResponse::Ok => {
            println!("daemon stopped");
            Ok(())
        }
        DaemonResponse::Error { message } => Err(anyhow::anyhow!(message)),
        other => Err(anyhow::anyhow!("unexpected response: {other:?}")),
    }
}

async fn restart(
    source: &bitrouter::paths::ConfigSource,
    socket: &Path,
    log_path: &Path,
) -> Result<()> {
    // Stop is best-effort — a missing daemon is fine, we just go straight to
    // start. Any other error from the running daemon is fatal.
    if socket.exists() {
        match daemon::send_command(socket, &DaemonCommand::Stop).await {
            Ok(DaemonResponse::Ok) => println!("daemon stopped"),
            Ok(DaemonResponse::Error { message }) => return Err(anyhow::anyhow!(message)),
            Ok(other) => return Err(anyhow::anyhow!("unexpected response: {other:?}")),
            Err(e) => tracing::warn!(error = %e, "stop failed — proceeding to start"),
        }
        //.2 allows in-flight requests up to 30s to drain. Wait that
        // long for the socket to be released. If it still isn't, escalate to
        // SIGKILL on the recorded pid — otherwise `start` would race the old
        // process for the same socket and one of them would die silently.
        let pid_path = pid_path_for(socket);
        if !wait_for_socket_release(socket, std::time::Duration::from_secs(30)).await {
            tracing::warn!("socket still held after 30s — escalating to SIGKILL on pid file");
            if let Some(pid) = daemon::read_pid_file(&pid_path).await {
                force_kill(pid).await;
            }
            // One more brief wait so the kernel cleans up the socket inode.
            wait_for_socket_release(socket, std::time::Duration::from_secs(2)).await;
            // The killed daemon never removed its pid file; do it now.
            daemon::remove_pid_file(&pid_path).await;
        }
    }
    start(source, log_path).await
}

/// Poll until the socket file is gone (the old daemon removes it on exit), up
/// to `timeout`. Returns true on success, false on timeout.
async fn wait_for_socket_release(socket: &Path, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !socket.exists() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    !socket.exists()
}

async fn reload(socket: &Path) -> Result<()> {
    // Snapshot every env-var-credentialed built-in provider's key from
    // *this* (CLI) process and hand them to the daemon along with the
    // reload command, so `export OPENAI_API_KEY=…; bitrouter reload`
    // propagates the new value into the running daemon instead of
    // requiring a full stop+start. The daemon writes them into its
    // env-override map before re-parsing config / re-running
    // zero-config provider detection.
    let env: Vec<(String, String)> = bitrouter_providers::zero_config_env_var_providers()
        .into_iter()
        .filter_map(|(_, var)| {
            std::env::var(var)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| (var.to_string(), v))
        })
        .collect();
    match daemon::send_command(socket, &DaemonCommand::Reload { env }).await? {
        DaemonResponse::Ok => {
            println!("config reloaded");
            Ok(())
        }
        DaemonResponse::Error { message } => Err(anyhow::anyhow!(message)),
        other => Err(anyhow::anyhow!("unexpected response: {other:?}")),
    }
}

async fn status(socket: &Path) -> Result<()> {
    let p = bitrouter::style::Palette::for_stdout();
    match daemon::send_command(socket, &DaemonCommand::Status).await {
        Ok(DaemonResponse::Status {
            pid,
            listen,
            models,
        }) => {
            print_status_running(&p, pid, &listen, models, socket);
            Ok(())
        }
        Ok(DaemonResponse::Error { message }) => Err(anyhow::anyhow!(message)),
        Ok(other) => Err(anyhow::anyhow!("unexpected response: {other:?}")),
        // No daemon listening on the socket → report stopped, not error.
        // Anything else (permission denied, malformed response, …) is a
        // real failure and bubbles to the pretty reporter.
        Err(e) if daemon::is_not_reachable(&e) => {
            print_status_stopped(&p, socket);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Render the running-daemon status block. Modelled on `systemctl
/// status` — a coloured bullet + headline, then a short indented list
/// of facts. Labels are dim so values are what the eye lands on.
fn print_status_running(
    p: &bitrouter::style::Palette,
    pid: u32,
    listen: &str,
    models: usize,
    socket: &Path,
) {
    println!(
        "{green}●{reset} bitrouter is {bold}running{reset}",
        green = p.green,
        bold = p.bold,
        reset = p.reset,
    );
    println!();
    print_status_row(p, "pid", &pid.to_string());
    print_status_row(p, "listen", listen);
    print_status_row(p, "models", &format!("{models} routable"));
    print_status_row(p, "socket", &socket.display().to_string());
}

/// Render the stopped-daemon status block. Hollow bullet (dim) +
/// headline, the socket we *would* connect to, and a one-line next
/// step. Exit code remains 0 — "stopped" is the answer to the
/// question, not a failure.
fn print_status_stopped(p: &bitrouter::style::Palette, socket: &Path) {
    println!(
        "{dim}○{reset} bitrouter is {bold}stopped{reset}",
        dim = p.dim,
        bold = p.bold,
        reset = p.reset,
    );
    println!();
    print_status_row(p, "socket", &socket.display().to_string());
    println!();
    println!(
        "  {dim}Run `bitrouter start` to launch the daemon.{reset}",
        dim = p.dim,
        reset = p.reset,
    );
}

/// One indented `label  value` row in a status block. The label column
/// is left-padded to 8 chars so columns line up for the typical labels
/// (`pid` / `listen` / `models` / `socket`).
fn print_status_row(p: &bitrouter::style::Palette, label: &str, value: &str) {
    println!(
        "  {dim}{label:<8}{reset}  {value}",
        dim = p.dim,
        reset = p.reset,
    );
}

async fn route(model: &str, source: &bitrouter::paths::ConfigSource, socket: &Path) -> Result<()> {
    // Try the running daemon first — its routing table reflects any `reload`s.
    if socket.exists() {
        match daemon::send_command(
            socket,
            &DaemonCommand::Route {
                model: model.into(),
            },
        )
        .await
        {
            Ok(DaemonResponse::Route { chain }) => {
                print_route_chain(model, &chain, "live daemon");
                return Ok(());
            }
            Ok(DaemonResponse::Error { message }) => return Err(anyhow::anyhow!(message)),
            Ok(other) => return Err(anyhow::anyhow!("unexpected response: {other:?}")),
            Err(e) => {
                // Fall through to the standalone resolution. The daemon may
                // just not be reachable from this client invocation.
                tracing::debug!(error = %e, "daemon route failed — resolving from config");
            }
        }
    }
    let cfg = bitrouter::paths::load_config(source).await?;
    let chain = commands::resolve_route(&cfg, model).await?;
    let label = if source.is_default() {
        "zero-config"
    } else {
        "config"
    };
    print_route_chain(model, &chain, label);
    Ok(())
}

fn print_route_chain(model: &str, chain: &[RouteHop], source: &str) {
    println!("model: {model}  (resolved via: {source})");
    if chain.is_empty() {
        println!("  (empty chain — no provider declares this model)");
        return;
    }
    for (i, hop) in chain.iter().enumerate() {
        println!(
            "  {}. {} → {} ({})",
            i + 1,
            hop.provider,
            hop.service_id,
            hop.api_protocol
        );
    }
}

// ===== management commands =====

async fn init(config_path: &Path) -> Result<()> {
    commands::init(config_path).await?;
    println!("wrote starter config to {}", config_path.display());
    println!("  (skip_auth is on — credential-less local requests are admitted)");
    Ok(())
}

async fn key(action: KeyAction) -> Result<()> {
    match action {
        KeyAction::Sign { user, db, policy } => {
            let key = commands::key_sign(&db, &user, policy.as_deref()).await?;
            println!("created virtual key {} for user '{user}'", key.id);
            println!();
            println!("  {}", key.secret);
            println!();
            println!("This secret is shown ONCE — only its SHA-256 hash is stored.");
            Ok(())
        }
    }
}

async fn models(source: &bitrouter::paths::ConfigSource, provider: Option<&str>) -> Result<()> {
    let cfg = bitrouter::paths::load_config(source).await?;
    let models = commands::list_models(&cfg, provider).await?;
    if models.is_empty() {
        match (provider, source.is_default()) {
            (Some(p), _) => println!("(no routable models for provider '{p}')"),
            (None, true) => {
                println!("(no routable models — zero-config mode and no provider env vars are set)")
            }
            (None, false) => println!("(no routable models — configure providers in your config)"),
        }
    }
    for (id, providers) in models {
        println!("{id}\t{}", providers.join(", "));
    }
    Ok(())
}

async fn policy(action: PolicyAction) -> Result<()> {
    match action {
        PolicyAction::Create { id, dir } => {
            let path = commands::create_policy(&dir, &id).await?;
            println!("wrote starter policy to {}", path.display());
            println!("  edit, then bind to a key with:");
            println!("    bitrouter key sign --user <id> --policy {id}");
            Ok(())
        }
    }
}

async fn providers(action: ProviderAction) -> Result<()> {
    match action {
        ProviderAction::List { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            let providers = commands::list_providers(&cfg);
            if providers.is_empty() {
                if source.is_default() {
                    println!("(no providers — zero-config mode and no provider env vars set)");
                } else {
                    println!("(no providers configured)");
                }
                return Ok(());
            }
            // header
            println!("{:<20} {:<8} {:<6} API_BASE", "ID", "MODELS", "ACTIVE");
            for p in providers {
                println!(
                    "{:<20} {:<8} {:<6} {}",
                    p.id,
                    p.model_count,
                    if p.active { "yes" } else { "no" },
                    p.api_base
                );
            }
            Ok(())
        }
        ProviderAction::Use { id } => {
            println!("v1 has no \"current provider\" — `providers use {id}` is a no-op.");
            println!(
                "  request a specific provider per-call via the model name or routing\n  prefs; bind a default by editing `bitrouter.yaml`."
            );
            Ok(())
        }
    }
}

async fn tools(action: ToolsAction) -> Result<()> {
    use bitrouter::tools as tools_cmd;

    match action {
        ToolsAction::List { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            if cfg.mcp_servers.is_empty() {
                println!("(no MCP servers configured)");
                println!("  add an `mcp_servers:` block to your bitrouter.yaml —");
                println!("  see the commented stub in the starter config written by");
                println!("  `bitrouter init`.");
                return Ok(());
            }
            let rows = tools_cmd::list(&cfg).await;
            for row in rows {
                match row.outcome {
                    Ok(tools) if tools.is_empty() => {
                        println!("{} (no tools advertised)", row.server);
                    }
                    Ok(tools) => {
                        println!("{} ({})", row.server, tools.len());
                        for t in tools {
                            if t.description.is_empty() {
                                println!("  {}", t.name);
                            } else {
                                println!("  {} — {}", t.name, t.description);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("{}: ERROR — {e}", row.server);
                    }
                }
            }
            Ok(())
        }
        ToolsAction::Status { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            if cfg.mcp_servers.is_empty() {
                println!("(no MCP servers configured)");
                return Ok(());
            }
            let rows = tools_cmd::status(&cfg).await;
            println!(
                "{:<24} {:<8} {:<12} TRANSPORT",
                "SERVER", "STATUS", "LATENCY"
            );
            for row in rows {
                let (status, latency) = match row.outcome {
                    Ok(d) => ("ok".to_string(), format!("{}ms", d.as_millis())),
                    Err(_) => ("FAIL".to_string(), "-".to_string()),
                };
                println!(
                    "{:<24} {:<8} {:<12} {}",
                    row.server, status, latency, row.transport
                );
                if let Err(e) = row.outcome.as_ref() {
                    eprintln!("  ↳ {e}");
                }
            }
            Ok(())
        }
        ToolsAction::Discover { server, config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            match tools_cmd::discover(&cfg, &server).await {
                Ok(yaml) => {
                    print!("{yaml}");
                    Ok(())
                }
                Err(e) => anyhow::bail!("discover '{server}': {e}"),
            }
        }
    }
}

async fn agent_proxy_cmd(agent: &str, source: &bitrouter::paths::ConfigSource) -> Result<()> {
    let cfg = bitrouter::paths::load_config(source).await?;
    bitrouter::agent_proxy::run(cfg, agent).await
}

// ===== observe =====

async fn observe(action: ObserveAction) -> Result<()> {
    match action {
        ObserveAction::Status {
            config,
            socket,
            json,
        } => {
            let socket = resolve_client_socket(config.as_deref(), socket.as_deref()).await?;
            observe_status(&socket, json).await
        }
    }
}

/// `bitrouter observe status` — ask the running daemon for the OTel
/// exporter snapshot, pretty-print (or JSON-dump) the result. When no
/// daemon is reachable, fall back to a "stopped" report that still
/// carries the compile-time `OTEL_ENABLED` flag so the user can tell
/// "feature off" from "daemon down."
async fn observe_status(socket: &Path, as_json: bool) -> Result<()> {
    use bitrouter_observe::OTEL_ENABLED;

    let payload = match daemon::send_command(socket, &DaemonCommand::ObserveStatus).await {
        Ok(DaemonResponse::ObserveStatus { payload }) => Some(payload),
        Ok(DaemonResponse::Error { message }) => return Err(anyhow::anyhow!(message)),
        Ok(other) => return Err(anyhow::anyhow!("unexpected response: {other:?}")),
        Err(e) if daemon::is_not_reachable(&e) => None,
        Err(e) => return Err(e),
    };

    if as_json {
        let snapshot =
            payload.unwrap_or_else(|| daemon::ObserveStatusPayload::unwired(OTEL_ENABLED));
        println!(
            "{}",
            serde_json::to_string_pretty(&snapshot).context("rendering observe status as JSON")?
        );
        return Ok(());
    }

    let p = bitrouter::style::Palette::for_stdout();
    match payload {
        Some(s) => print_observe_running(&p, &s, socket),
        None => print_observe_stopped(&p, socket),
    }
    Ok(())
}

fn print_observe_running(
    p: &bitrouter::style::Palette,
    s: &daemon::ObserveStatusPayload,
    socket: &Path,
) {
    let (bullet, headline) = if s.exporter_wired {
        (
            format!("{green}●{reset}", green = p.green, reset = p.reset),
            "OTel exporter is wired",
        )
    } else if s.compiled_in {
        (
            format!("{dim}○{reset}", dim = p.dim, reset = p.reset),
            "OTel feature compiled in, exporter not configured",
        )
    } else {
        (
            format!("{dim}○{reset}", dim = p.dim, reset = p.reset),
            "OTel feature not compiled in",
        )
    };
    println!(
        "{bullet} bitrouter observe — {bold}{headline}{reset}",
        bold = p.bold,
        reset = p.reset,
    );
    println!();
    print_status_row(p, "compiled", if s.compiled_in { "yes" } else { "no" });
    print_status_row(p, "wired", if s.exporter_wired { "yes" } else { "no" });
    if let Some(endpoint) = &s.endpoint {
        print_status_row(p, "endpoint", endpoint);
    }
    if let Some(service) = &s.service_name {
        print_status_row(p, "service", service);
    }
    if let Some(sampler) = &s.sampler {
        let val = match s.sampler_arg {
            Some(arg) => format!("{sampler} (arg={arg})"),
            None => sampler.clone(),
        };
        print_status_row(p, "sampler", &val);
    }
    print_status_row(p, "metrics", if s.metrics_enabled { "on" } else { "off" });
    print_status_row(p, "headers", &s.header_count.to_string());
    print_status_row(p, "res-attrs", &s.resource_attribute_count.to_string());
    print_status_row(
        p,
        "api-keys",
        &format!("{} / {}", s.api_key_count, s.api_key_cap),
    );
    print_status_row(
        p,
        "users",
        &format!("{} / {}", s.user_id_count, s.user_id_cap),
    );
    print_status_row(p, "in-flight", &s.active_spans.to_string());
    print_status_row(p, "socket", &socket.display().to_string());
}

fn print_observe_stopped(p: &bitrouter::style::Palette, socket: &Path) {
    use bitrouter_observe::OTEL_ENABLED;
    println!(
        "{dim}○{reset} bitrouter observe — {bold}daemon stopped{reset}",
        dim = p.dim,
        bold = p.bold,
        reset = p.reset,
    );
    println!();
    print_status_row(p, "compiled", if OTEL_ENABLED { "yes" } else { "no" });
    print_status_row(p, "socket", &socket.display().to_string());
    println!();
    println!(
        "  {dim}Run `bitrouter start` to launch the daemon, then re-run this command.{reset}",
        dim = p.dim,
        reset = p.reset,
    );
}

async fn agents_cmd(action: AgentsAction) -> Result<()> {
    use bitrouter::agents as agents_cmd;

    match action {
        AgentsAction::List { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            let rows = agents_cmd::list(&cfg);
            println!(
                "{:<16} {:<12} {:<10} DESCRIPTION",
                "ID", "CONFIGURED", "CATALOG"
            );
            for row in rows {
                println!(
                    "{:<16} {:<12} {:<10} {}",
                    row.id,
                    if row.configured { "yes" } else { "no" },
                    if row.in_catalog { "yes" } else { "no" },
                    row.description,
                );
            }
            Ok(())
        }
        AgentsAction::Check { config } => {
            let source = bitrouter::paths::resolve_config(config.as_deref())?;
            let cfg = bitrouter::paths::load_config(&source).await?;
            if cfg.agents.is_empty() {
                println!("(no agents configured)");
                println!("  install one with: bitrouter agents install <id>");
                return Ok(());
            }
            let rows = agents_cmd::check(&cfg).await;
            println!("{:<24} {:<8} LATENCY/ERROR", "AGENT", "STATUS");
            for row in rows {
                match row.outcome {
                    Ok(d) => println!("{:<24} {:<8} {}ms", row.id, "ok", d.as_millis()),
                    Err(e) => {
                        println!("{:<24} {:<8} -", row.id, "FAIL");
                        eprintln!("  ↳ {e}");
                    }
                }
            }
            Ok(())
        }
        AgentsAction::Install { id } => match agents_cmd::install(&id) {
            Ok(yaml) => {
                print!("{yaml}");
                Ok(())
            }
            Err(e) => anyhow::bail!(e),
        },
    }
}

// ===== helpers =====

/// Derive the pid file path that matches a control-socket path: same
/// directory, same stem, `.pid` extension. (Both default to `./bitrouter.*`.)
fn pid_path_for(socket: &Path) -> PathBuf {
    let mut p = socket.to_path_buf();
    p.set_extension("pid");
    p
}

fn print_unimplemented(name: &str, detail: &str) {
    eprintln!("`bitrouter {name}` is not implemented in v1.0.");
    eprintln!();
    for line in detail.lines() {
        eprintln!("  {line}");
    }
}

/// Liveness check: `kill -0 <pid>` returns success iff the pid is reachable
/// (i.e. exists and we have permission to signal it). No actual signal is
/// sent. We shell out to keep `apps/bitrouter` `#![forbid(unsafe_code)]`.
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Send SIGKILL to `pid`. Best-effort — if the process is already gone the
/// kernel returns ESRCH and we silently move on.
async fn force_kill(pid: u32) {
    let _ = tokio::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}
