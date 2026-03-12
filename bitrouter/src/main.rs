mod cli;
mod init;
mod runtime;
#[cfg(feature = "tui")]
mod tui;

use std::path::PathBuf;

use crate::runtime::{AppRuntime, PathOverrides, resolve_home};
use bitrouter_core::jwt::claims::{BudgetScope, TokenScope};
use clap::{Parser, Subcommand};

type DefaultRuntime = AppRuntime<bitrouter_config::ConfigRoutingTable>;

#[derive(Debug, Parser)]
#[command(name = "bitrouter", version, about = "BitRouter CLI")]
struct Cli {
    /// BitRouter home directory (overrides automatic resolution)
    #[arg(long, global = true)]
    home_dir: Option<PathBuf>,

    /// Path to config file (overrides <home>/bitrouter.yaml)
    #[arg(long, global = true)]
    config_file: Option<PathBuf>,

    /// Path to .env file (overrides <home>/.env)
    #[arg(long, global = true)]
    env_file: Option<PathBuf>,

    /// Path to runtime directory (overrides <home>/run)
    #[arg(long, global = true)]
    run_dir: Option<PathBuf>,

    /// Path to logs directory (overrides <home>/logs)
    #[arg(long, global = true)]
    logs_dir: Option<PathBuf>,

    /// Database connection URL (overrides env vars and config file)
    #[arg(long = "db", global = true)]
    database_url: Option<String>,

    /// Run server without the TUI (headless mode)
    #[arg(long)]
    headless: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Interactive setup wizard
    Init,
    /// Start the API server (foreground)
    Serve,
    /// Start as background daemon
    Start,
    /// Stop the daemon
    Stop,
    /// Show runtime status
    Status,
    /// Restart the daemon
    Restart,
    /// Hot-reload configuration without restarting the daemon
    Reload,

    /// Manage local web3 account keypairs
    Account {
        /// Generate a new web3 master key and set as active
        #[arg(short, long)]
        generate_key: bool,

        /// List all local account keys
        #[arg(short, long)]
        list: bool,

        /// Set active account by index or pubkey prefix
        #[arg(long)]
        set: Option<String>,
    },

    /// Sign a JWT with the active master key
    Keygen {
        /// Chain to sign with: "solana" or "base"
        #[arg(long, default_value = "solana")]
        chain: String,

        /// Token scope: admin or api
        #[arg(long, default_value = "api")]
        scope: String,

        /// Expiration duration (e.g., "5m", "1h", "30d", "never")
        #[arg(long)]
        exp: Option<String>,

        /// Comma-separated list of allowed model patterns
        #[arg(long, value_delimiter = ',')]
        models: Option<Vec<String>>,

        /// Budget limit in micro USD
        #[arg(long)]
        budget: Option<u64>,

        /// Budget scope: session or account
        #[arg(long)]
        budget_scope: Option<String>,

        /// Budget range (e.g., "rounds:10", "duration:3600s")
        #[arg(long)]
        budget_range: Option<String>,

        /// Optional label for saving the token locally
        #[arg(long)]
        name: Option<String>,
    },

    /// Manage locally-stored JWTs for the active account
    Keys {
        /// List saved tokens
        #[arg(short, long)]
        list: bool,

        /// Show decoded claims of a token (by name or index)
        #[arg(long)]
        show: Option<String>,

        /// Remove a saved token (by name or index)
        #[arg(long)]
        rm: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Resolve paths early — init needs them but not a loaded runtime
    let paths = resolve_home(cli.home_dir.as_deref());
    let overrides = PathOverrides {
        config_file: cli.config_file.clone(),
        env_file: cli.env_file.clone(),
        runtime_dir: cli.run_dir.clone(),
        log_dir: cli.logs_dir.clone(),
    };
    let paths = overrides.apply(paths);

    // Handle init before loading runtime
    if matches!(cli.command, Some(Command::Init)) {
        init::run_init(&paths)?;
        return Ok(());
    }

    // Handle key management commands — these only need paths, not a full runtime.
    let keys_dir = paths.home_dir.join(".keys");
    match cli.command {
        Some(Command::Account {
            generate_key,
            list,
            set,
        }) => {
            cli::account::run(&keys_dir, generate_key, list, set)?;
            return Ok(());
        }
        Some(Command::Keygen {
            chain,
            scope,
            exp,
            models,
            budget,
            budget_scope,
            budget_range,
            name,
        }) => {
            let scope = match scope.as_str() {
                "admin" => TokenScope::Admin,
                "api" => TokenScope::Api,
                other => {
                    return Err(
                        format!("invalid scope \"{other}\" — use \"admin\" or \"api\"").into(),
                    );
                }
            };
            let budget_scope = budget_scope
                .as_deref()
                .map(|s| match s {
                    "session" => Ok(BudgetScope::Session),
                    "account" => Ok(BudgetScope::Account),
                    other => Err(format!(
                        "invalid budget scope \"{other}\" — use \"session\" or \"account\""
                    )),
                })
                .transpose()?;
            let opts = cli::keygen::KeygenOpts {
                chain,
                scope,
                exp,
                models,
                budget,
                budget_scope,
                budget_range,
                name,
            };
            cli::keygen::run(&keys_dir, opts)?;
            return Ok(());
        }
        Some(Command::Keys { list, show, rm }) => {
            cli::keys::run(&keys_dir, list, show, rm)?;
            return Ok(());
        }
        _ => {}
    }

    let use_tui = cli.command.is_none() && !cli.headless;

    // Skip tracing init when TUI owns the terminal — logs corrupt the alternate screen
    if !use_tui {
        init_tracing();
    }

    let mut runtime: DefaultRuntime = DefaultRuntime::load(paths.clone())
        .unwrap_or_else(|_| DefaultRuntime::scaffold(paths.clone()));

    // Auto-init: when launching in TUI mode with no providers, run the setup
    // wizard first so the user lands in a fully configured TUI.
    if use_tui && !runtime.config.has_configured_providers() {
        let is_interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
        if is_interactive {
            eprintln!();
            eprintln!("  No providers configured. Starting setup wizard...");
            eprintln!();

            match init::run_init(&paths) {
                Ok(init::InitOutcome::Configured) => {
                    // Reload runtime with the newly written config
                    runtime = DefaultRuntime::load(paths.clone())
                        .unwrap_or_else(|_| DefaultRuntime::scaffold(paths.clone()));
                }
                Ok(init::InitOutcome::Cancelled) => {
                    // User cancelled — fall through to TUI with empty state
                }
                Err(e) => {
                    eprintln!("  Setup wizard failed: {e}");
                    eprintln!("  Continuing with empty configuration...");
                    eprintln!();
                }
            }
        }
    }

    // First-run guidance
    if !use_tui {
        print_first_run_guidance(&runtime);
    }

    // Connect to database for commands that start the server.
    let serves = cli.command.is_none() || matches!(cli.command, Some(Command::Serve));
    if serves {
        let env_file = paths.env_file.exists().then_some(paths.env_file.as_path());
        let db_url = crate::runtime::resolve_database_url(
            cli.database_url.as_deref(),
            &runtime.config,
            &paths.home_dir,
            env_file,
        );
        match sea_orm::Database::connect(&db_url).await {
            Ok(db) => {
                if let Err(e) = crate::runtime::migrate(&db).await {
                    tracing::warn!("database migration failed: {e}");
                }
                runtime.db = Some(std::sync::Arc::new(db));
            }
            Err(e) => {
                tracing::warn!("database connection failed ({db_url}): {e}");
            }
        }
    }

    match cli.command {
        None => run_default(runtime, cli.headless).await?,
        Some(Command::Serve) => {
            let model_router = crate::runtime::Router::new(
                reqwest::Client::new(),
                runtime.config.providers.clone(),
            );
            runtime.serve(model_router).await?
        }
        Some(Command::Start) => runtime.start().await?,
        Some(Command::Stop) => runtime.stop().await?,
        Some(Command::Status) => {
            let status = runtime.status();
            match status.daemon_pid {
                Some(pid) => println!("daemon:    running (pid {pid})"),
                None => println!("daemon:    stopped"),
            }
            println!("home:      {}", status.home_dir.display());
            println!("config:    {}", status.config_file.display());
            println!("runtime:   {}", status.runtime_dir.display());
            println!("listen:    {}", status.listen_addr);
            println!("providers: {}", status.providers.join(", "));
            if !status.models.is_empty() {
                println!("models:    {}", status.models.join(", "));
            }
        }
        Some(Command::Restart) => runtime.restart().await?,
        Some(Command::Reload) => runtime.reload().await?,
        Some(
            Command::Init | Command::Account { .. } | Command::Keygen { .. } | Command::Keys { .. },
        ) => {
            unreachable!()
        }
    }

    Ok(())
}

fn print_first_run_guidance(runtime: &DefaultRuntime) {
    if runtime.config.has_configured_providers() {
        return;
    }

    let detected = bitrouter_config::detect_providers_from_env();
    if detected.is_empty() {
        eprintln!("No providers configured and no API keys found in environment.");
        eprintln!("Run `bitrouter init` to set up providers interactively.");
        eprintln!();
    } else {
        let names: Vec<&str> = detected.iter().map(|d| d.name.as_str()).collect();
        eprintln!(
            "Auto-detected providers from environment: {}",
            names.join(", ")
        );
        eprintln!("Direct routing is available (e.g., \"openai:gpt-4o\").");
        eprintln!("Run `bitrouter init` to save a permanent configuration.");
        eprintln!();
    }
}

async fn run_default(
    runtime: DefaultRuntime,
    headless: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let status = runtime.status();

    let model_router =
        crate::runtime::Router::new(reqwest::Client::new(), runtime.config.providers.clone());

    if headless {
        runtime.serve(model_router).await?;
        return Ok(());
    }

    #[cfg(feature = "tui")]
    {
        let tui_config = crate::tui::TuiConfig {
            listen_addr: status.listen_addr,
            providers: vec![], // TODO: populate from config
            route_count: 0,    // TODO: populate from routing table
            daemon_pid: status.daemon_pid,
        };

        tokio::select! {
            result = runtime.serve(model_router) => {
                if let Err(e) = result {
                    tracing::error!("server error: {e}");
                }
            }
            result = crate::tui::run(tui_config) => {
                result?;
            }
        }
    }

    #[cfg(not(feature = "tui"))]
    {
        let _ = status;
        runtime.serve(model_router).await?;
    }

    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}
