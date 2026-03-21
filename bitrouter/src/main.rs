mod cli;
mod init;
mod runtime;
#[cfg(feature = "tui")]
mod tui;

use std::path::PathBuf;

use crate::runtime::{AppRuntime, PathOverrides, resolve_home};
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

    /// A2A agent management and protocol client
    #[cfg(feature = "a2a")]
    A2a {
        #[command(subcommand)]
        action: A2aAction,
    },

    /// Manage MCP tools on a running daemon
    #[cfg(feature = "mcp")]
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

    /// Master-wallet signing operations with Swig
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

#[cfg(feature = "a2a")]
#[derive(Debug, Subcommand)]
enum A2aAction {
    /// Discover a remote agent by fetching its Agent Card
    Discover {
        /// Base URL of the remote agent (e.g., https://agent.example.com)
        url: String,
    },
    /// Send a task to a remote agent
    Send {
        /// Base URL of the remote agent
        url: String,
        /// Message to send
        #[arg(long)]
        message: String,
    },
    /// Check the status of a task
    Status {
        /// Base URL of the remote agent
        url: String,
        /// Task ID to check
        #[arg(long)]
        task: String,
    },
    /// Cancel a running task
    Cancel {
        /// Base URL of the remote agent
        url: String,
        /// Task ID to cancel
        #[arg(long)]
        task: String,
    },
    /// List tasks from a remote agent
    ListTasks {
        /// Base URL of the remote agent
        url: String,
    },
}

#[cfg(feature = "mcp")]
#[derive(Debug, Subcommand)]
enum ToolsAction {
    /// List all tools from the running daemon
    List,
    /// Update the tool filter for an upstream
    Filter {
        /// Upstream server name
        server: String,
        /// Allow list (comma-separated tool names)
        #[arg(long, value_delimiter = ',')]
        allow: Option<Vec<String>>,
        /// Deny list (comma-separated tool names)
        #[arg(long, value_delimiter = ',')]
        deny: Option<Vec<String>>,
    },
    /// List upstream servers
    Upstreams,
    /// List configured access groups
    Groups,
}

#[derive(Debug, Subcommand)]
enum SudoAction {
    /// Create a Swig embedded wallet (requires master wallet signature)
    CreateEmbeddedWallet,
    /// Derive an agent wallet with spend limits (requires master wallet signature)
    DeriveAgentWallet {
        /// Maximum tokens per transaction (lamports)
        #[arg(long)]
        per_tx_cap: Option<u64>,

        /// Cumulative spending cap (lamports)
        #[arg(long)]
        cumulative_cap: Option<u64>,

        /// Expiration (e.g., "7d", "30d", "never")
        #[arg(long)]
        expiration: Option<String>,

        /// Human-readable label for this agent wallet
        #[arg(long, default_value = "default")]
        label: Option<String>,
    },
    /// Update agent wallet permissions (requires master wallet signature)
    SetPermissions {
        /// Agent wallet address (uses persisted address if omitted)
        #[arg(long)]
        agent: Option<String>,

        /// Maximum tokens per transaction (lamports)
        #[arg(long)]
        per_tx_cap: Option<u64>,

        /// Cumulative spending cap (lamports)
        #[arg(long)]
        cumulative_cap: Option<u64>,

        /// Expiration (e.g., "7d", "30d", "never")
        #[arg(long)]
        expiration: Option<String>,
    },
    /// Display wallet info and persisted policy (no signing required)
    ShowWallet,
    /// List all locally-tracked agent wallets
    ListAgents,
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
                mcp_groups: bitrouter_mcp::groups::McpAccessGroups::default(),
            };
            cli::keygen::run(&keys_dir, opts)?;
            return Ok(());
        }
        #[cfg(feature = "a2a")]
        Some(Command::A2a { action }) => {
            match action {
                A2aAction::Discover { url } => cli::a2a::run_discover(&url).await?,
                A2aAction::Send { url, message } => cli::a2a::run_send(&url, &message).await?,
                A2aAction::Status { url, task } => cli::a2a::run_status(&url, &task).await?,
                A2aAction::Cancel { url, task } => cli::a2a::run_cancel(&url, &task).await?,
                A2aAction::ListTasks { url } => cli::a2a::run_list_tasks(&url).await?,
            }
            return Ok(());
        }
        #[cfg(feature = "mcp")]
        Some(Command::Tools { action }) => {
            let runtime: DefaultRuntime = DefaultRuntime::load(paths.clone())
                .unwrap_or_else(|_| DefaultRuntime::scaffold(paths.clone()));
            let addr = runtime.config.server.listen;
            match action {
                ToolsAction::List => cli::tools::run_list(&keys_dir, addr)?,
                ToolsAction::Filter {
                    server,
                    allow,
                    deny,
                } => cli::tools::run_filter(&keys_dir, addr, &server, allow, deny)?,
                ToolsAction::Upstreams => cli::tools::run_upstreams(&keys_dir, addr)?,
                ToolsAction::Groups => cli::tools::run_groups(&keys_dir, addr)?,
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
                SudoAction::CreateEmbeddedWallet => {
                    cli::sudo::run_create_embedded_wallet(home)?;
                }
                SudoAction::DeriveAgentWallet {
                    per_tx_cap,
                    cumulative_cap,
                    expiration,
                    label,
                } => {
                    cli::sudo::run_derive_agent_wallet(
                        home,
                        per_tx_cap,
                        cumulative_cap,
                        expiration,
                        label,
                    )?;
                }
                SudoAction::SetPermissions {
                    agent,
                    per_tx_cap,
                    cumulative_cap,
                    expiration,
                } => {
                    cli::sudo::run_set_permissions(
                        home,
                        agent,
                        per_tx_cap,
                        cumulative_cap,
                        expiration,
                    )?;
                }
                SudoAction::ShowWallet => {
                    cli::sudo::run_show_wallet(home)?;
                }
                SudoAction::ListAgents => {
                    cli::sudo::run_list_agents(home)?;
                }
            }
            return Ok(());
        }
        Some(Command::Route { action }) => {
            // Route commands talk to a running daemon, so we only need the
            // config to know the listen address.
            let runtime: DefaultRuntime = DefaultRuntime::load(paths.clone())
                .unwrap_or_else(|_| DefaultRuntime::scaffold(paths.clone()));
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

    let mut runtime: DefaultRuntime = DefaultRuntime::load(paths.clone())
        .unwrap_or_else(|_| DefaultRuntime::scaffold(paths.clone()));

    // ── First-run guidance ────────────────────────────────────────
    // On serve/start, if onboarding hasn't been completed, print a message
    // directing the user to run `bitrouter init` instead of auto-triggering.
    let is_server_start =
        cli.command.is_none() || matches!(cli.command, Some(Command::Serve | Command::Start));
    if is_server_start && cli::onboarding::should_onboard(&paths.home_dir) {
        let is_interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
        if is_interactive {
            eprintln!();
            eprintln!("  No wallet configured. Run `bitrouter init` to set up your node.");
            eprintln!();
        }
    }

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
        None => run_default(runtime).await?,
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
        _ => {
            // All other commands (Init, A2a, Account, Keygen, Keys, Route,
            // Sudo, Tools) are handled above and return early.
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

async fn run_default(runtime: DefaultRuntime) -> Result<(), Box<dyn std::error::Error>> {
    let status = runtime.status();

    let model_router =
        crate::runtime::Router::new(reqwest::Client::new(), runtime.config.providers.clone());

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

fn has_removed_headless_flag<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    args.into_iter().any(|arg| arg.as_ref() == "--headless")
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
/// Detects existing onboarding state and offers Cloud vs BYOK mode selection.
/// - Cloud: delegates to [`cli::onboarding::run_onboarding`], then writes cloud
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
        OnboardingStatus::CompletedCloud | OnboardingStatus::CompletedByok => {
            let label = match state.status {
                OnboardingStatus::CompletedCloud => "Cloud (Swig wallet)",
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
            // Fall through to mode selection — user can retry cloud or switch to BYOK
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
        "Cloud — use BitRouter Cloud Node with x402 payments (requires Solana wallet)",
        "BYOK  — bring your own API keys (OpenAI, Anthropic, Google, custom)",
    ];

    let selection = Select::with_theme(&theme)
        .with_prompt("Setup mode")
        .items(choices)
        .default(0)
        .interact()?;

    match selection {
        // ── Cloud path ────────────────────────────────────────
        0 => {
            match cli::onboarding::run_onboarding(home)? {
                cli::onboarding::OnboardingOutcome::CompletedCloud { rpc_url } => {
                    if let Err(e) = write_cloud_provider_config(paths, &rpc_url) {
                        eprintln!("  Warning: failed to write cloud config: {e}");
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

/// Write a cloud provider config entry to `bitrouter.yaml` after onboarding.
///
/// This generates a provider block for BitRouter Cloud Node directly in the
/// config file (not yet registered in the builtin provider registry).
fn write_cloud_provider_config(
    paths: &crate::runtime::RuntimePaths,
    rpc_url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::fs;

    let config_path = &paths.config_file;

    // Read existing config or start fresh.
    let existing = fs::read_to_string(config_path).unwrap_or_default();

    // Parse YAML to check if cloud provider is already configured.
    if let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&existing)
        && value
            .get("providers")
            .and_then(|p| p.get("bitrouter-cloud"))
            .is_some()
    {
        return Ok(());
    }

    let cloud_block = format!(
        "\n\
        # Solana RPC endpoint for Swig wallet operations\n\
        solana_rpc_url: \"{rpc_url}\"\n\n\
        # BitRouter Cloud Node (added by onboarding)\n\
        # Uses x402 for request payments — only a wallet is needed.\n\
        providers:\n\
        \x20 bitrouter-cloud:\n\
        \x20   api_base: \"https://cloud.bitrouter.ai/v1\"\n\
        \x20   api_protocol: openai\n\
        \x20   auth:\n\
        \x20     type: x402\n\n\
        models:\n\
        \x20 default:\n\
        \x20   strategy: priority\n\
        \x20   endpoints:\n\
        \x20     - provider: bitrouter-cloud\n"
    );

    // Append cloud config to existing file.
    let mut content = existing;
    content.push_str(&cloud_block);

    fs::create_dir_all(&paths.home_dir).map_err(|e| format!("failed to create home dir: {e}"))?;
    fs::write(config_path, content)
        .map_err(|e| format!("failed to write {}: {e}", config_path.display()))?;

    Ok(())
}
