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
use bitrouter::daemon::{self, DEFAULT_CONTROL_SOCKET, DaemonCommand, DaemonResponse, RouteHop};
use bitrouter_sdk::caller::PaymentMethod;
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
        /// Path to `bitrouter.yaml`.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
    },
    /// Spawn `bitrouter serve` as a detached background process.
    Start {
        /// Path to `bitrouter.yaml` (passed through to the child).
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
        /// Path to redirect the daemon's stdout/stderr to.
        #[arg(long, default_value = "./bitrouter.log")]
        log: PathBuf,
    },
    /// Send a `stop` command to a running daemon.
    Stop {
        /// Control socket path.
        #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
        socket: PathBuf,
    },
    /// `stop` then `start` — config path is passed through.
    Restart {
        /// Path to `bitrouter.yaml`.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
        /// Control socket path.
        #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
        socket: PathBuf,
        /// Path to redirect the new daemon's stdout/stderr to.
        #[arg(long, default_value = "./bitrouter.log")]
        log: PathBuf,
    },
    /// Hot-reload the running daemon's config / routing table.
    Reload {
        /// Control socket path.
        #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
        socket: PathBuf,
    },
    /// Report a running daemon's status (pid, listen address, model count).
    Status {
        /// Control socket path.
        #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
        socket: PathBuf,
    },
    /// Resolve a model name through the routing table. Uses the running
    /// daemon if reachable, otherwise loads the config and resolves locally.
    Route {
        /// The model name to resolve.
        model: String,
        /// Path to `bitrouter.yaml` (used as the standalone fallback).
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
        /// Control socket path.
        #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
        socket: PathBuf,
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
        /// Path to `bitrouter.yaml`.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
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
    /// Log in to an upstream provider. Today: `bitrouter login github-copilot`
    /// runs the GitHub OAuth Device Authorization Grant + stores the
    /// resulting token under `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`.
    /// Cloud login (no argument) is not in v1.0 scope.
    Login {
        /// Provider id to log in to (e.g. `github-copilot`). Omit for the
        /// v0-style cloud login flow (not implemented in v1.0).
        provider: Option<String>,
    },
    /// Log out of an upstream provider — clears the OAuth token from disk.
    /// Cloud logout (no argument) is not in v1.0 scope.
    Logout {
        /// Provider id whose stored OAuth token should be removed.
        provider: Option<String>,
    },
    /// Print the authenticated cloud identity — not implemented in v1.0.
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
        /// Path to `bitrouter.yaml`.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum AgentsAction {
    /// Show the bundled v1.0 catalog of well-known agents and which of
    /// them are present under `agents:` in the loaded config.
    List {
        /// Path to `bitrouter.yaml`.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
    },
    /// Spawn each configured agent and verify it answers `initialize`.
    Check {
        /// Path to `bitrouter.yaml`.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
    },
    /// Print a YAML stub for an agent in the catalog (paste under
    /// `agents:` in `bitrouter.yaml`).
    Install {
        /// Agent id from the catalog (see `bitrouter agents list`).
        id: String,
    },
}

#[derive(Subcommand)]
enum ToolsAction {
    /// List tools advertised by every configured MCP server.
    List {
        /// Path to `bitrouter.yaml`.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
    },
    /// Health-check every configured MCP server with a `tools/list` round-trip.
    Status {
        /// Path to `bitrouter.yaml`.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
    },
    /// Connect to one MCP server and print a YAML stub suitable for pasting
    /// into `mcp_servers:`.
    Discover {
        /// Server id (must exist under `mcp_servers` in the config).
        server: String,
        /// Path to `bitrouter.yaml`.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
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
        /// Database URL.
        #[arg(short, long, default_value = "sqlite://./bitrouter.db")]
        db: String,
        /// Funding model for the key (`credits` / `mpp` / `byok` / `none`).
        #[arg(short, long, default_value = "credits")]
        payment_method: String,
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
        /// Path to `bitrouter.yaml`.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
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
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve { config } => serve(&config).await,
        Command::Start { config, log } => start(&config, &log).await,
        Command::Stop { socket } => stop(&socket).await,
        Command::Restart {
            config,
            socket,
            log,
        } => restart(&config, &socket, &log).await,
        Command::Reload { socket } => reload(&socket).await,
        Command::Status { socket } => status(&socket).await,
        Command::Route {
            model,
            config,
            socket,
        } => route(&model, &config, &socket).await,
        Command::Init { config } => init(&config).await,
        Command::Key { action } => key(action).await,
        Command::Models { config, provider } => models(&config, provider.as_deref()).await,
        Command::Tools { action } => tools(action).await,
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
        Command::Login { provider } => match provider.as_deref() {
            Some(name) => bitrouter::commands::login_provider(name).await,
            None => {
                print_unimplemented(
                    "login",
                    "Cloud login is not in v1.0 scope. Run\n\
                     `bitrouter login <provider>` for per-provider OAuth (today: github-copilot),\n\
                     or mint a local virtual key with `bitrouter key sign --user <id>`.",
                );
                Ok(())
            }
        },
        Command::Logout { provider } => match provider.as_deref() {
            Some(name) => bitrouter::commands::logout_provider(name).await,
            None => {
                print_unimplemented(
                    "logout",
                    "See `bitrouter logout <provider>` for per-provider OAuth logout.\n\
                     No cloud session in v1.0.",
                );
                Ok(())
            }
        },
        Command::Whoami => {
            print_unimplemented(
                "whoami",
                "Cloud identity is not in v1.0 scope. Local callers are identified\n\
                 by the `brvk_` virtual key (see `bitrouter key sign`).",
            );
            Ok(())
        }
        Command::Agents { action } => agents_cmd(action).await,
        Command::AgentProxy { agent, config } => agent_proxy_cmd(&agent, &config).await,
    }
}

// ===== serve / daemon control =====

/// Fan out a daemon `Reload` (and SIGHUP) to every reloadable subsystem the
/// running daemon owns. Failures from any single subsystem are accumulated and
/// reported together so an unrelated subsystem (e.g. a missing policy dir)
/// doesn't mask a fixable routing-table reload.
struct AppReloader {
    app: Arc<bitrouter_sdk::App>,
    policy_store: Arc<bitrouter_policy::PolicyStore>,
}

#[async_trait::async_trait]
impl daemon::DaemonReloader for AppReloader {
    async fn reload(&self) -> anyhow::Result<()> {
        let mut errors: Vec<String> = Vec::new();
        if let Some(pipeline) = self.app.language_model() {
            if let Err(e) = pipeline.routing_table().reload().await {
                errors.push(format!("routing table: {e}"));
            }
        }
        if let Err(e) = self.policy_store.reload().await {
            errors.push(format!("policy store: {e}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(errors.join("; ")))
        }
    }
}

async fn serve(config_path: &Path) -> Result<()> {
    let cfg = config::load(config_path)
        .await
        .with_context(|| format!("loading {}", config_path.display()))?;
    let listen = cfg.server.listen.clone();
    let socket_path = PathBuf::from(&cfg.server.control_socket);
    let pid_path = pid_path_for(&socket_path);

    let assembled = bitrouter::build_app_with_path(&cfg, Some(config_path)).await?;
    let app = Arc::new(assembled.app);
    let policy_store = assembled.policy_store;
    let reloader: Arc<dyn daemon::DaemonReloader> = Arc::new(AppReloader {
        app: app.clone(),
        policy_store: policy_store.clone(),
    });

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
    let control = daemon::run_control_socket(socket_path, app.clone(), listen, reloader.clone());

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

    let result = tokio::select! {
        r = http => r,
        r = control => r,
        // SIGHUP loop never returns Ok by design; an error from signal setup
        // is logged and we keep serving.
        r = hup => match r {
            Ok(()) => Ok(()),
            Err(e) => { tracing::warn!(error = %e, "SIGHUP listener unavailable"); Ok(()) }
        },
    };
    daemon::remove_pid_file(&pid_path).await;
    result
}

async fn start(config_path: &Path, log_path: &Path) -> Result<()> {
    // Refuse to start a second daemon on top of a live one — silent overlap
    // would race two `serve`s for the same socket and one would die into the
    // log file (the user wouldn't see it).
    let cfg_socket_path = match config::load(config_path).await {
        Ok(cfg) => Some(PathBuf::from(&cfg.server.control_socket)),
        Err(_) => None,
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
    let mut child = std::process::Command::new(&exe)
        .arg("serve")
        .arg("--config")
        .arg(config_path)
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
        anyhow::bail!(
            "daemon exited immediately ({status}); see {} for details",
            log_path.display()
        );
    }

    println!(
        "bitrouter daemon started (pid {}) — logs at {}",
        child.id(),
        log_path.display()
    );
    Ok(())
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

async fn restart(config_path: &Path, socket: &Path, log_path: &Path) -> Result<()> {
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
    start(config_path, log_path).await
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
    match daemon::send_command(socket, &DaemonCommand::Reload).await? {
        DaemonResponse::Ok => {
            println!("config reloaded");
            Ok(())
        }
        DaemonResponse::Error { message } => Err(anyhow::anyhow!(message)),
        other => Err(anyhow::anyhow!("unexpected response: {other:?}")),
    }
}

async fn status(socket: &Path) -> Result<()> {
    match daemon::send_command(socket, &DaemonCommand::Status).await? {
        DaemonResponse::Status {
            pid,
            listen,
            models,
        } => {
            println!("running       : yes");
            println!("pid           : {pid}");
            println!("http listen   : {listen}");
            println!("routable models: {models}");
            Ok(())
        }
        DaemonResponse::Error { message } => Err(anyhow::anyhow!(message)),
        other => Err(anyhow::anyhow!("unexpected response: {other:?}")),
    }
}

async fn route(model: &str, config_path: &Path, socket: &Path) -> Result<()> {
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
    let cfg = config::load(config_path)
        .await
        .with_context(|| format!("loading {}", config_path.display()))?;
    let chain = commands::resolve_route(&cfg, model).await?;
    print_route_chain(model, &chain, "config");
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
        KeyAction::Sign {
            user,
            db,
            payment_method,
            policy,
        } => {
            let pm = match payment_method.as_str() {
                "credits" => PaymentMethod::Credits,
                "mpp" => PaymentMethod::Mpp,
                "byok" => PaymentMethod::Byok,
                "none" => PaymentMethod::None,
                other => anyhow::bail!("unknown payment method '{other}'"),
            };
            let key = commands::key_sign(&db, &user, pm, policy.as_deref()).await?;
            println!("created virtual key {} for user '{user}'", key.id);
            println!();
            println!("  {}", key.secret);
            println!();
            println!("This secret is shown ONCE — only its SHA-256 hash is stored.");
            Ok(())
        }
    }
}

async fn models(config_path: &Path, provider: Option<&str>) -> Result<()> {
    let cfg = config::load(config_path)
        .await
        .with_context(|| format!("loading {}", config_path.display()))?;
    let models = commands::list_models(&cfg, provider).await?;
    if models.is_empty() {
        match provider {
            Some(p) => println!("(no routable models for provider '{p}')"),
            None => println!(
                "(no routable models — configure providers in {})",
                config_path.display()
            ),
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
            let cfg = config::load(&config)
                .await
                .with_context(|| format!("loading {}", config.display()))?;
            let providers = commands::list_providers(&cfg);
            if providers.is_empty() {
                println!("(no providers configured in {})", config.display());
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
            let cfg = config::load(&config)
                .await
                .with_context(|| format!("loading {}", config.display()))?;
            if cfg.mcp_servers.is_empty() {
                println!("(no MCP servers configured in {})", config.display());
                println!("  add a `mcp_servers:` block — see the commented stub in the");
                println!("  starter config written by `bitrouter init`.");
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
            let cfg = config::load(&config)
                .await
                .with_context(|| format!("loading {}", config.display()))?;
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
            let cfg = config::load(&config)
                .await
                .with_context(|| format!("loading {}", config.display()))?;
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

async fn agent_proxy_cmd(agent: &str, config_path: &Path) -> Result<()> {
    let cfg = config::load(config_path)
        .await
        .with_context(|| format!("loading {}", config_path.display()))?;
    bitrouter::agent_proxy::run(cfg, agent).await
}

async fn agents_cmd(action: AgentsAction) -> Result<()> {
    use bitrouter::agents as agents_cmd;

    match action {
        AgentsAction::List { config } => {
            let cfg = config::load(&config)
                .await
                .with_context(|| format!("loading {}", config.display()))?;
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
            let cfg = config::load(&config)
                .await
                .with_context(|| format!("loading {}", config.display()))?;
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
