mod cli;
mod init;
mod runtime;
#[cfg(feature = "tui")]
mod tui;

use std::path::PathBuf;

use crate::runtime::{AppRuntime, PathOverrides, RuntimePaths, resolve_home};
use bitrouter_core::auth::claims::{BudgetScope, TokenScope};
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

        /// Comma-separated list of allowed tool patterns (e.g., "github/*,jira/search")
        #[arg(long, value_delimiter = ',')]
        tools: Option<Vec<String>>,

        /// Optional label for saving the token locally
        #[arg(long)]
        name: Option<String>,
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

    /// Wallet status and diagnostics
    Sudo {
        #[command(subcommand)]
        action: SudoAction,
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
enum SudoAction {
    /// Display wallet info and status
    ShowWallet,
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
        run_unified_init(&paths)?;
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
            tools,
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
                tools,
                budget,
                budget_scope,
                budget_range,
                name,
                mcp_groups: bitrouter_core::routers::upstream::ToolServerAccessGroups::default(),
            };
            cli::keygen::run(&keys_dir, opts)?;
            return Ok(());
        }
        Some(Command::Agents { action }) => {
            let runtime: DefaultRuntime = load_or_warn_scaffold(&paths);
            let addr = runtime.config.server.listen;
            match action {
                AgentsAction::List => cli::agents::run_list(&keys_dir, addr)?,
                AgentsAction::Status => cli::agents::run_status(&keys_dir, addr)?,
            }
            return Ok(());
        }
        Some(Command::Tools { action }) => {
            let runtime: DefaultRuntime = load_or_warn_scaffold(&paths);
            let addr = runtime.config.server.listen;
            match action {
                ToolsAction::List => cli::tools::run_list(&keys_dir, addr)?,
                ToolsAction::Status => cli::tools::run_status(&keys_dir, addr)?,
            }
            return Ok(());
        }
        Some(Command::Keys { list, show, rm }) => {
            cli::keys::run(&keys_dir, list, show, rm)?;
            return Ok(());
        }
        Some(Command::Sudo { action }) => {
            let home = &paths.home_dir;
            match action {
                SudoAction::ShowWallet => {
                    cli::sudo::run_show_wallet(home)?;
                }
            }
            return Ok(());
        }
        Some(Command::Route { action }) => {
            // Route commands talk to a running daemon, so we only need the
            // config to know the listen address.
            let runtime: DefaultRuntime = load_or_warn_scaffold(&paths);
            let addr = runtime.config.server.listen;
            match action {
                RouteAction::List => cli::route::run_list(&keys_dir, addr)?,
                RouteAction::Add {
                    model,
                    endpoints,
                    strategy,
                } => cli::route::run_add(
                    &keys_dir,
                    addr,
                    cli::route::RouteAddOpts {
                        model,
                        endpoints,
                        strategy: Some(strategy),
                    },
                )?,
                RouteAction::Rm { model } => cli::route::run_remove(&keys_dir, addr, &model)?,
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

    // ── First-run: auto-launch unified init ───────────────────────
    // When onboarding has never been completed and stdin is a terminal,
    // launch the Node/BYOK setup wizard inline so the user is fully
    // configured before the server starts.
    let is_server_start =
        cli.command.is_none() || matches!(cli.command, Some(Command::Serve | Command::Start));
    if is_server_start && cli::onboarding::should_onboard(&paths.home_dir) {
        let is_interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
        if is_interactive {
            match run_unified_init(&paths) {
                Ok(()) => {
                    // Pause so the user can read onboarding output before TUI takes over.
                    if use_tui {
                        eprint!("  Press Enter to continue...");
                        let _ = std::io::stdin().read_line(&mut String::new());
                    }
                    runtime = load_or_warn_scaffold(&paths);
                }
                Err(e) => {
                    eprintln!("  Setup wizard failed: {e}");
                    eprintln!("  Continuing with empty configuration...");
                    eprintln!("  Run `bitrouter init` anytime to retry.");
                    eprintln!();
                }
            }
        } else {
            // Non-interactive: print guidance instead of launching the wizard
            print_first_run_guidance(&runtime);
        }
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
        None => run_default(runtime).await?,
        Some(Command::Serve) => {
            let base_client = reqwest::Client::new();
            let mut model_router = crate::runtime::Router::new(
                reqwest_middleware::ClientBuilder::new(base_client.clone()).build(),
                runtime.config.providers.clone(),
            );
            if let Some(x402_client) = build_x402_client_from_state(&runtime.paths, &runtime.config)
            {
                model_router = model_router.with_x402_client(x402_client);
            }
            #[cfg(feature = "mpp-tempo")]
            if let Some(mpp_client) = build_mpp_client_from_state(&runtime.paths, &runtime.config) {
                model_router = model_router.with_mpp_client(mpp_client);
            }
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
            // All other commands (Init, A2a, Account, Keygen, Keys, Route,
            // Sudo, Tools) are handled above and return early.
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
    let status = runtime.status();

    let base_client = reqwest::Client::new();
    let mut model_router = crate::runtime::Router::new(
        reqwest_middleware::ClientBuilder::new(base_client.clone()).build(),
        runtime.config.providers.clone(),
    );
    if let Some(x402_client) = build_x402_client_from_state(&runtime.paths, &runtime.config) {
        model_router = model_router.with_x402_client(x402_client);
    }
    #[cfg(feature = "mpp-tempo")]
    if let Some(mpp_client) = build_mpp_client_from_state(&runtime.paths, &runtime.config) {
        model_router = model_router.with_mpp_client(mpp_client);
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

fn has_removed_headless_flag<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    args.into_iter().any(|arg| arg.as_ref() == "--headless")
}

/// Build an x402 payment client from onboarding state, if a wallet is configured.
///
/// Returns `None` when no wallet has been set up (BYOK mode).
/// Logs a warning and returns `None` on load failure so the server can still
/// start for non-x402 providers.
fn build_x402_client_from_state(
    paths: &crate::runtime::RuntimePaths,
    config: &bitrouter_config::BitrouterConfig,
) -> Option<reqwest_middleware::ClientWithMiddleware> {
    // Check if any provider uses x402 auth.
    let x402_providers: Vec<&str> = config
        .providers
        .iter()
        .filter(|(_, p)| matches!(&p.auth, Some(bitrouter_config::AuthConfig::X402)))
        .map(|(name, _)| name.as_str())
        .collect();

    if x402_providers.is_empty() {
        return None;
    }

    let keys_dir = paths.home_dir.join(".keys");
    let (_prefix, keypair) = match cli::account::load_active_keypair(&keys_dir) {
        Ok(kp) => kp,
        Err(e) => {
            tracing::warn!(
                providers = ?x402_providers,
                "x402 providers configured but no keypair available: {e}",
            );
            return None;
        }
    };

    let rpc_url = config.solana_rpc_url.as_deref().unwrap_or_else(|| {
        tracing::warn!("no Solana RPC URL configured, falling back to mainnet-beta",);
        "https://api.mainnet-beta.solana.com"
    });

    let client = crate::runtime::x402::build_x402_client_from_master(
        &keypair,
        rpc_url,
        reqwest::Client::new(),
        true,
    );
    tracing::info!(
        rpc = %rpc_url,
        providers = ?x402_providers,
        "x402 payment signer loaded",
    );
    Some(client)
}

/// Build an MPP payment client from the active keypair, if any MPP providers are configured.
///
/// Returns `None` when no wallet has been set up or no providers use MPP auth.
/// Logs a warning and returns `None` on failure so the server can still start.
#[cfg(feature = "mpp-tempo")]
fn build_mpp_client_from_state(
    paths: &crate::runtime::RuntimePaths,
    config: &bitrouter_config::BitrouterConfig,
) -> Option<reqwest_middleware::ClientWithMiddleware> {
    let mpp_providers: Vec<&str> = config
        .providers
        .iter()
        .filter(|(_, p)| matches!(&p.auth, Some(bitrouter_config::AuthConfig::Mpp)))
        .map(|(name, _)| name.as_str())
        .collect();

    if mpp_providers.is_empty() {
        return None;
    }

    let keys_dir = paths.home_dir.join(".keys");
    let (_prefix, keypair) = match cli::account::load_active_keypair(&keys_dir) {
        Ok(kp) => kp,
        Err(e) => {
            tracing::warn!(
                providers = ?mpp_providers,
                "MPP providers configured but no keypair available: {e}",
            );
            return None;
        }
    };

    let rpc_url = config
        .mpp
        .as_ref()
        .and_then(|m| m.networks.tempo.as_ref())
        .and_then(|t| t.rpc_url.as_deref())
        .unwrap_or(crate::runtime::mpp_client::DEFAULT_TEMPO_RPC_URL);

    match crate::runtime::mpp_client::build_mpp_client(
        &keypair,
        rpc_url,
        reqwest::Client::new(),
        true,
    ) {
        Ok(client) => {
            tracing::info!(
                rpc = %rpc_url,
                providers = ?mpp_providers,
                "MPP payment signer loaded",
            );
            Some(client)
        }
        Err(e) => {
            tracing::warn!("failed to load MPP signer: {e} — MPP providers will be unavailable",);
            None
        }
    }
}

/// Build a Solana session MPP client from the active keypair, if any MPP providers are configured.
#[cfg(feature = "mpp-solana")]
fn build_mpp_solana_client_from_state(
    paths: &crate::runtime::RuntimePaths,
    config: &bitrouter_config::BitrouterConfig,
) -> Option<reqwest_middleware::ClientWithMiddleware> {
    let mpp_providers: Vec<&str> = config
        .providers
        .iter()
        .filter(|(_, p)| matches!(&p.auth, Some(bitrouter_config::AuthConfig::Mpp)))
        .map(|(name, _)| name.as_str())
        .collect();

    if mpp_providers.is_empty() {
        return None;
    }

    let keys_dir = paths.home_dir.join(".keys");
    let (_prefix, keypair) = match cli::account::load_active_keypair(&keys_dir) {
        Ok(kp) => kp,
        Err(e) => {
            tracing::warn!(
                providers = ?mpp_providers,
                "Solana MPP providers configured but no keypair available: {e}",
            );
            return None;
        }
    };

    match crate::runtime::mpp_solana_client::build_mpp_solana_client(
        &keypair,
        reqwest::Client::new(),
    ) {
        Ok(client) => {
            tracing::info!(
                providers = ?mpp_providers,
                "Solana MPP session signer loaded",
            );
            Some(client)
        }
        Err(e) => {
            tracing::warn!(
                "failed to load Solana MPP signer: {e} — MPP providers will be unavailable",
            );
            None
        }
    }
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

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}

/// Unified `bitrouter init` entry point.
///
/// Detects existing onboarding state and offers Node vs BYOK mode selection.
/// - Node: delegates to [`cli::onboarding::run_onboarding`], then writes node
///   provider config.
/// - BYOK: delegates to [`init::run_init`], then writes `onboarding.json` with
///   `completed_byok` status.
fn run_unified_init(
    paths: &crate::runtime::RuntimePaths,
) -> Result<(), Box<dyn std::error::Error>> {
    use cli::onboarding::{OnboardingStatus, load_state, save_state};
    use dialoguer::{Select, theme::ColorfulTheme};

    let theme = ColorfulTheme::default();
    let home = &paths.home_dir;

    // ── Idempotency: detect existing state ────────────────────
    let state = load_state(home);
    match state.status {
        OnboardingStatus::CompletedNode | OnboardingStatus::CompletedByok => {
            let label = match state.status {
                OnboardingStatus::CompletedNode => "BitRouter Node (MPP wallet)",
                _ => "BYOK (bring your own keys)",
            };
            println!();
            println!("  Onboarding already completed: {label}");
            println!();

            let choices = &["Reconfigure from scratch", "Exit"];
            let selection = Select::with_theme(&theme)
                .with_prompt("What would you like to do?")
                .items(choices)
                .default(1)
                .interact()?;

            if selection == 1 {
                return Ok(());
            }
            // Fall through to re-run mode selection
        }
        OnboardingStatus::FailedRecoverable => {
            println!();
            println!("  Previous onboarding attempt failed. Resuming...");
            println!();
            // Fall through to mode selection — user can retry or switch to BYOK
        }
        OnboardingStatus::NotStarted | OnboardingStatus::Deferred => {
            // First run or previously deferred — proceed normally
        }
    }

    // ── Mode selection ────────────────────────────────────────
    println!();
    println!("  BitRouter Setup");
    println!("  ───────────────");
    println!();
    println!("  Choose how to connect to LLM providers:");
    println!();

    let choices = &[
        "BitRouter Node — pay per request with Tempo MPP (auto-generates web3 wallet)",
        "BYOK  — bring your own API keys (OpenAI, Anthropic, Google, custom)",
    ];

    let selection = Select::with_theme(&theme)
        .with_prompt("Setup mode")
        .items(choices)
        .default(0)
        .interact()?;

    match selection {
        // ── Node path ────────────────────────────────────────
        0 => {
            match cli::onboarding::run_onboarding(home)? {
                cli::onboarding::OnboardingOutcome::CompletedNode { .. } => {
                    if let Err(e) = write_node_provider_config(paths) {
                        eprintln!("  Warning: failed to write node config: {e}");
                    }
                }
                cli::onboarding::OnboardingOutcome::CompletedByok => {
                    // User switched to BYOK during onboarding (wallet skip)
                }
                cli::onboarding::OnboardingOutcome::Deferred => {
                    // Will re-prompt on next run
                }
            }
        }
        // ── BYOK path ────────────────────────────────────────
        _ => {
            match init::run_init(paths)? {
                init::InitOutcome::Configured => {
                    let mut state = load_state(home);
                    state.status = OnboardingStatus::CompletedByok;
                    save_state(home, &state)?;
                }
                init::InitOutcome::Cancelled => {
                    // User cancelled — write deferred so we re-prompt
                    let mut state = load_state(home);
                    if state.status == OnboardingStatus::NotStarted {
                        state.status = OnboardingStatus::Deferred;
                        save_state(home, &state)?;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Write a node provider config entry to `bitrouter.yaml` after onboarding.
///
/// Since `bitrouter` is a builtin provider (api_base and api_protocol
/// come from the registry), the user config only needs to supply auth.
fn write_node_provider_config(
    paths: &crate::runtime::RuntimePaths,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::fs;

    let config_path = &paths.config_file;

    // Read existing config or start fresh.
    let existing = fs::read_to_string(config_path).unwrap_or_default();

    // Parse YAML to check if node provider is already configured.
    if let Ok(value) = serde_saphyr::from_str::<serde_json::Value>(&existing)
        && value
            .get("providers")
            .and_then(|p| p.get("bitrouter"))
            .is_some()
    {
        return Ok(());
    }

    let node_block = "\n\
        # BitRouter Node (added by onboarding)\n\
        # Uses MPP (Machine Payment Protocol) on Tempo for request payments.\n\
        # Fund your EVM wallet on Tempo: https://app.tempo.xyz\n\
        #\n\
        # Route requests with: bitrouter:<model>\n\
        # Example: bitrouter:gpt-4o\n\
        providers:\n\
        \x20 bitrouter:\n\
        \x20   auth:\n\
        \x20     type: mpp\n";

    // Append node config to existing file.
    let mut content = existing;
    content.push_str(node_block);

    fs::create_dir_all(&paths.home_dir).map_err(|e| format!("failed to create home dir: {e}"))?;
    fs::write(config_path, content)
        .map_err(|e| format!("failed to write {}: {e}", config_path.display()))?;

    Ok(())
}
