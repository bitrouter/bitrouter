//! `bitrouter` CLI entry point — a thin shell over the `bitrouter` lib.
//!
//! Subcommand surface (007 §1.1): `serve` / `start` / `stop` / `restart` /
//! `reload` / `status` / `route` / `init` / `key sign` / `models` / `tools` /
//! `policy create` / `providers (list|use)` / `wallet` / `login` / `logout` /
//! `whoami` / `agents`. Daemon control runs over a Unix socket (007 §6.1) —
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
    /// Tool registry — v1.0 routes tool calls but does not maintain a global
    /// tool registry; per-request tools live in the request body.
    Tools,
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
    /// Cloud login — not implemented in v1.0 (no cloud dependency in v1.0
    /// scope; sign a `brvk_` virtual key with `bitrouter key sign` instead).
    Login,
    /// Cloud logout — not implemented in v1.0.
    Logout,
    /// Print the authenticated cloud identity — not implemented in v1.0.
    Whoami,
    /// ACP agent management — the v1.0 `acp` module is pure-routing only.
    Agents,
}

#[derive(Subcommand)]
enum KeyAction {
    /// Mint a new `brvk_` virtual key for a user (007 §1.2: `bitrouter key
    /// sign`). v1 does not sign a JWT — it creates a DB-backed virtual key and
    /// prints the plaintext once.
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
    /// Write a starter policy file to the policy dir (007 §1.1).
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
        Command::Tools => {
            print_unimplemented(
                "tools",
                "v1.0 has no global tool registry — request bodies carry their own\n\
                 tool list and `PolicyHook` (004 §4) gates them by name.",
            );
            Ok(())
        }
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
        Command::Login => {
            print_unimplemented(
                "login",
                "Cloud login is not in v1.0 scope. Mint a local virtual key with\n\
                 `bitrouter key sign --user <id>` and configure clients with it.",
            );
            Ok(())
        }
        Command::Logout => {
            print_unimplemented(
                "logout",
                "See `bitrouter login` — no cloud session in v1.0.",
            );
            Ok(())
        }
        Command::Whoami => {
            print_unimplemented(
                "whoami",
                "Cloud identity is not in v1.0 scope. Local callers are identified\n\
                 by the `brvk_` virtual key (see `bitrouter key sign`).",
            );
            Ok(())
        }
        Command::Agents => {
            print_unimplemented(
                "agents",
                "The v1.0 `acp` module is pure-routing — agent lifecycle management\n\
                 is not in v1.0 scope.",
            );
            Ok(())
        }
    }
}

// ===== serve / daemon control =====

async fn serve(config_path: &Path) -> Result<()> {
    let cfg = config::load(config_path)
        .await
        .with_context(|| format!("loading {}", config_path.display()))?;
    let listen = cfg.server.listen.clone();
    let socket_path = PathBuf::from(&cfg.server.control_socket);
    let pid_path = pid_path_for(&socket_path);

    let assembled = bitrouter::build_app_with_path(&cfg, Some(config_path)).await?;
    let app = Arc::new(assembled.app);

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
    let control = daemon::run_control_socket(socket_path, app.clone(), listen);

    let result = tokio::select! {
        r = http => r,
        r = control => r,
    };
    daemon::remove_pid_file(&pid_path).await;
    result
}

async fn start(config_path: &Path, log_path: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("locating current bitrouter binary")?;
    // Open the log file (append) for the child's stdout+stderr.
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening daemon log {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .context("duplicating log handle for stderr")?;

    let child = std::process::Command::new(&exe)
        .arg("serve")
        .arg("--config")
        .arg(config_path)
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err))
        .stdin(std::process::Stdio::null())
        .spawn()
        .context("spawning detached `bitrouter serve`")?;

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
        // Give the old process a beat to release the socket / port.
        wait_for_socket_release(socket).await;
    }
    start(config_path, log_path).await
}

/// Poll until the socket file is gone (the old daemon removes it on exit), up
/// to a small ceiling. Bounded so a stuck daemon doesn't wedge `restart`.
async fn wait_for_socket_release(socket: &Path) {
    for _ in 0..20 {
        if !socket.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
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
