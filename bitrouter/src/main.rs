#![recursion_limit = "256"]

mod cli;
mod init;
mod runtime;
#[cfg(feature = "tui")]
mod tui;

use std::path::PathBuf;

use crate::runtime::{AppRuntime, PathOverrides, RuntimePaths, resolve_home};
use clap::{Parser, Subcommand};

type DefaultRuntime = AppRuntime<
    bitrouter_core::routers::dynamic::DynamicRoutingTable<bitrouter_config::ConfigRoutingTable>,
>;

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
    /// Hot-reload the configuration file
    Reload,

    /// Manage runtime routes (requires a running daemon)
    Route {
        #[command(subcommand)]
        action: RouteAction,
    },

    /// Manage OWS wallets
    Wallet {
        #[command(subcommand)]
        action: WalletAction,
    },

    /// Manage OWS API keys for agent access
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },

    /// Inspect upstream agents on a running daemon
    Agents {
        #[command(subcommand)]
        action: AgentsAction,
    },

    /// Inspect MCP tools on a running daemon
    Tools {
        #[command(subcommand)]
        action: ToolsAction,
    },
}

#[derive(Debug, Subcommand)]
enum RouteAction {
    /// List all routes (config-defined + dynamic)
    List,
    /// Add or update a dynamic route
    Add {
        /// Virtual model name (e.g., "research", "fast")
        model: String,

        /// Endpoints in "provider:model_id" format (at least one required)
        #[arg(required = true, num_args = 1..)]
        endpoints: Vec<String>,

        /// Routing strategy: "priority" or "load_balance"
        #[arg(long, default_value = "priority")]
        strategy: String,
    },
    /// Remove a dynamic route
    Rm {
        /// Model name to remove
        model: String,
    },
}

#[derive(Debug, Subcommand)]
enum AgentsAction {
    /// List configured upstream agents
    List,
    /// Show upstream agent connection health
    Status,
}

#[derive(Debug, Subcommand)]
enum ToolsAction {
    /// List all tools from the running daemon
    List,
    /// Show upstream MCP server health
    Status,
}

#[derive(Debug, Subcommand)]
enum WalletAction {
    /// Create a new wallet with a fresh BIP-39 mnemonic
    Create {
        /// Wallet name
        #[arg(long)]
        name: String,

        /// Mnemonic word count (12 or 24)
        #[arg(long, default_value = "12")]
        words: u32,

        /// Display the mnemonic phrase after creation
        #[arg(long)]
        show_mnemonic: bool,
    },
    /// Import a wallet from a mnemonic phrase
    Import {
        /// Wallet name
        #[arg(long)]
        name: String,

        /// Import from mnemonic phrase (prompted interactively)
        #[arg(long)]
        mnemonic: bool,

        /// Import from a hex private key (prompted interactively)
        #[arg(long)]
        private_key: bool,

        /// Chain hint for private-key import (e.g. "evm", "solana")
        #[arg(long)]
        chain: Option<String>,

        /// HD derivation index (mnemonic import only)
        #[arg(long)]
        index: Option<u32>,
    },
    /// List all wallets
    List,
    /// Show detailed wallet info
    Info {
        /// Wallet name or ID
        #[arg(long)]
        wallet: String,
    },
    /// Export a wallet's mnemonic phrase
    Export {
        /// Wallet name or ID
        #[arg(long)]
        wallet: String,
    },
    /// Delete a wallet
    Delete {
        /// Wallet name or ID
        #[arg(long)]
        wallet: String,
    },
    /// Rename a wallet
    Rename {
        /// Current wallet name or ID
        #[arg(long)]
        wallet: String,

        /// New wallet name
        #[arg(long)]
        new_name: String,
    },
}

#[derive(Debug, Subcommand)]
enum KeyAction {
    /// Create a new API key for agent access
    Create {
        /// Key name (e.g. "claude-agent")
        #[arg(long)]
        name: String,

        /// Wallet name(s) this key can access
        #[arg(long, required = true, num_args = 1..)]
        wallet: Vec<String>,

        /// Policy ID(s) to attach
        #[arg(long)]
        policy: Vec<String>,

        /// Expiration timestamp (ISO 8601)
        #[arg(long)]
        expires_at: Option<String>,
    },
    /// List all API keys
    List,
    /// Revoke an API key
    Revoke {
        /// Key ID to revoke
        #[arg(long)]
        id: String,
    },
    /// Sign a JWT for agent access (operator mints tokens for agents)
    Sign {
        /// OWS wallet name to sign with (operator wallet)
        #[arg(long)]
        wallet: String,

        /// Model name patterns the agent may access (comma-separated)
        #[arg(long, value_delimiter = ',')]
        models: Option<Vec<String>>,

        /// Budget limit in micro USD (1 USD = 1,000,000 μUSD)
        #[arg(long)]
        budget: Option<u64>,

        /// Budget scope: "session" or "account"
        #[arg(long)]
        budget_scope: Option<String>,

        /// Expiration duration (e.g. "30d", "12h", "3600s", or raw seconds)
        #[arg(long)]
        exp: Option<String>,

        /// OWS agent key ID to bind to this token
        #[arg(long)]
        ows_key: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if has_removed_headless_flag(std::env::args_os()) {
        return Err(
            "`bitrouter --headless` has been removed; use `bitrouter serve` to run the server in the foreground."
                .into(),
        );
    }

    let cli = Cli::parse();

    // Skip update check in TUI mode — the alternate screen would hide it.
    let use_tui = cli.command.is_none() && cfg!(feature = "tui");
    let update_check = if use_tui {
        None
    } else {
        Some(tokio::spawn(cli::update_check::check_for_update()))
    };

    let result = run_cli(cli).await;

    // Print update notice (if available) after the command finishes.
    if let Some(handle) = update_check
        && let Ok(Ok(Some(msg))) =
            tokio::time::timeout(std::time::Duration::from_secs(2), handle).await
    {
        eprintln!("{msg}");
    }

    result
}

async fn run_cli(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
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

    // Handle wallet and key management — these only need the OWS vault, not a runtime.
    match cli.command {
        Some(Command::Wallet { action }) => {
            match action {
                WalletAction::Create {
                    name,
                    words,
                    show_mnemonic,
                } => cli::wallet::create(&name, Some(words), show_mnemonic)?,
                WalletAction::Import {
                    name,
                    mnemonic,
                    private_key,
                    chain,
                    index,
                } => {
                    if mnemonic {
                        cli::wallet::import_mnemonic(&name, index)?;
                    } else if private_key {
                        cli::wallet::import_private_key(&name, chain.as_deref())?;
                    } else {
                        return Err("specify --mnemonic or --private-key for wallet import".into());
                    }
                }
                WalletAction::List => cli::wallet::list(None)?,
                WalletAction::Info { wallet } => cli::wallet::info(&wallet, None)?,
                WalletAction::Export { wallet } => cli::wallet::export(&wallet)?,
                WalletAction::Delete { wallet } => cli::wallet::delete(&wallet)?,
                WalletAction::Rename { wallet, new_name } => {
                    cli::wallet::rename(&wallet, &new_name)?
                }
            }
            return Ok(());
        }
        Some(Command::Key { action }) => {
            match action {
                KeyAction::Create {
                    name,
                    wallet,
                    policy,
                    expires_at,
                } => cli::key::create(&name, &wallet, &policy, expires_at.as_deref())?,
                KeyAction::List => cli::key::list()?,
                KeyAction::Revoke { id } => cli::key::revoke(&id)?,
                KeyAction::Sign {
                    wallet,
                    models,
                    budget,
                    budget_scope,
                    exp,
                    ows_key,
                } => cli::key::sign(
                    &wallet,
                    models.as_deref(),
                    budget,
                    budget_scope.as_deref(),
                    exp.as_deref(),
                    ows_key.as_deref(),
                )?,
            }
            return Ok(());
        }
        Some(Command::Agents { action }) => {
            let runtime: DefaultRuntime = load_or_warn_scaffold(&paths);
            let addr = runtime.config.server.listen;
            match action {
                AgentsAction::List => cli::agents::run_list(&runtime.config, addr)?,
                AgentsAction::Status => cli::agents::run_status(&runtime.config, addr)?,
            }
            return Ok(());
        }
        Some(Command::Tools { action }) => {
            let runtime: DefaultRuntime = load_or_warn_scaffold(&paths);
            let addr = runtime.config.server.listen;
            match action {
                ToolsAction::List => cli::tools::run_list(&runtime.config, addr)?,
                ToolsAction::Status => cli::tools::run_status(&runtime.config, addr)?,
            }
            return Ok(());
        }
        Some(Command::Route { action }) => {
            let runtime: DefaultRuntime = load_or_warn_scaffold(&paths);
            let addr = runtime.config.server.listen;
            match action {
                RouteAction::List => cli::route::run_list(&runtime.config, addr)?,
                RouteAction::Add {
                    model,
                    endpoints,
                    strategy,
                } => cli::route::run_add(
                    &runtime.config,
                    addr,
                    cli::route::RouteAddOpts {
                        model,
                        endpoints,
                        strategy: Some(strategy),
                    },
                )?,
                RouteAction::Rm { model } => cli::route::run_remove(&runtime.config, addr, &model)?,
            }
            return Ok(());
        }
        _ => {}
    }

    let use_tui = cli.command.is_none() && cfg!(feature = "tui");

    // Skip tracing init when TUI owns the terminal — logs corrupt the alternate screen
    if !use_tui {
        init_tracing();
    }

    let mut runtime: DefaultRuntime = load_or_warn_scaffold(&paths);

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

    // When an OWS wallet is configured and OWS_PASSPHRASE is not already set,
    // prompt interactively (if a TTY is attached) or warn the user.
    let starts_server =
        cli.command.is_none() || matches!(cli.command, Some(Command::Serve | Command::Start));
    if starts_server && let Err(e) = ensure_ows_passphrase(&runtime.config) {
        eprintln!("wallet passphrase error: {e}");
        std::process::exit(1);
    }

    match cli.command {
        None => run_default(runtime).await?,
        Some(Command::Serve) => {
            print_first_run_guidance(&runtime);
            let base_client = reqwest::Client::new();
            let model_router = crate::runtime::Router::new(
                reqwest_middleware::ClientBuilder::new(base_client).build(),
                runtime.config.providers.clone(),
            );
            runtime.serve_with_reload(model_router).await?
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
        Some(Command::Reload) => runtime.reload()?,
        _ => {
            // All other commands (Init, Route, Wallet, Key, Tools, Agents)
            // are handled above and return early.
            unreachable!()
        }
    }

    Ok(())
}

/// Load config from disk, warning on stderr if the file exists but fails to
/// parse (and falling back to scaffold defaults).
fn load_or_warn_scaffold(paths: &RuntimePaths) -> DefaultRuntime {
    match DefaultRuntime::load(paths.clone()) {
        Ok(rt) => rt,
        Err(e) => {
            if paths.config_file.exists() {
                eprintln!(
                    "warning: failed to parse {}: {e}",
                    paths.config_file.display()
                );
                eprintln!("         falling back to default configuration");
                eprintln!();
            }
            DefaultRuntime::scaffold(paths.clone())
        }
    }
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

async fn run_default(runtime: DefaultRuntime) -> Result<(), Box<dyn std::error::Error>> {
    print_first_run_guidance(&runtime);
    let status = runtime.status();

    let base_client = reqwest::Client::new();
    let model_router = crate::runtime::Router::new(
        reqwest_middleware::ClientBuilder::new(base_client).build(),
        runtime.config.providers.clone(),
    );
    #[cfg(feature = "tui")]
    {
        let tui_config = crate::tui::TuiConfig {
            listen_addr: status.listen_addr,
            providers: vec![], // TODO: populate from config
            route_count: 0,    // TODO: populate from routing table
            daemon_pid: status.daemon_pid,
        };

        tokio::select! {
            result = runtime.serve_with_reload(model_router) => {
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
        runtime.serve_with_reload(model_router).await?;
    }

    Ok(())
}

/// Ensure `OWS_PASSPHRASE` is available when the config includes an OWS wallet.
///
/// Resolution order:
/// 1. `OWS_PASSPHRASE` env var already set → use as-is (non-interactive).
/// 2. Interactive TTY available → prompt with `dialoguer::Password`.
/// 3. Neither → return an error so the caller can exit gracefully.
fn ensure_ows_passphrase(
    config: &bitrouter_config::BitrouterConfig,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let wallet = match config.wallet.as_ref() {
        Some(w) => w,
        None => return Ok(()),
    };

    if std::env::var("OWS_PASSPHRASE").is_ok() {
        return Ok(());
    }

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err(format!(
            "wallet '{}' configured but OWS_PASSPHRASE is not set and stdin is not a terminal",
            wallet.name,
        )
        .into());
    }

    let passphrase = dialoguer::Password::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt(format!("OWS passphrase for wallet '{}'", wallet.name))
        .allow_empty_password(true)
        .interact()?;

    // SAFETY: single-threaded at this point (before tokio runtime enters serve).
    unsafe { std::env::set_var("OWS_PASSPHRASE", passphrase) };

    Ok(())
}

fn has_removed_headless_flag<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    args.into_iter().any(|arg| arg.as_ref() == "--headless")
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use clap::error::ErrorKind;

    #[test]
    fn serve_subcommand_parses_correctly() {
        let cli = Cli::try_parse_from(["bitrouter", "serve"]).ok();
        assert!(cli.is_some());
        assert!(matches!(
            cli,
            Some(Cli {
                command: Some(Command::Serve),
                ..
            })
        ));
    }

    #[test]
    fn headless_flag_is_rejected() {
        let err = Cli::try_parse_from(["bitrouter", "--headless"]).err();
        assert!(matches!(
            err.as_ref().map(clap::Error::kind),
            Some(ErrorKind::UnknownArgument)
        ));
    }

    #[test]
    fn help_mentions_serve_but_not_headless() {
        let mut command = Cli::command();
        let mut help = Vec::new();
        assert!(command.write_long_help(&mut help).is_ok());

        let help_text = String::from_utf8(help).ok();
        assert!(help_text.is_some());
        assert!(matches!(help_text.as_deref(), Some(text) if text.contains("serve")));
        assert!(matches!(help_text.as_deref(), Some(text) if !text.contains("--headless")));
    }

    #[test]
    fn removed_headless_flag_is_detected_before_parse() {
        assert!(has_removed_headless_flag(["bitrouter", "--headless"]));
        assert!(!has_removed_headless_flag(["bitrouter", "serve"]));
    }
}
