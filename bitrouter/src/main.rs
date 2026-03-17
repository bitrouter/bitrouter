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

        /// Optional label for saving the token locally
        #[arg(long)]
        name: Option<String>,
    },

    /// A2A agent management and protocol client
    A2a {
        #[command(subcommand)]
        action: A2aAction,
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

#[derive(Debug, Subcommand)]
enum A2aAction {
    /// Register a local agent card
    Register {
        /// Agent name (lowercase alphanumeric + hyphens)
        #[arg(long)]
        name: Option<String>,

        /// Import full Agent Card from JSON file
        #[arg(long, conflicts_with_all = ["description", "version", "provider_org"])]
        card: Option<String>,

        /// Agent description
        #[arg(long)]
        description: Option<String>,

        /// Agent version
        #[arg(long, default_value = "0.1.0")]
        version: String,

        /// Provider organization name
        #[arg(long)]
        provider_org: Option<String>,

        /// Bind to JWT iss claim (CAIP-10 address)
        #[arg(long)]
        iss: Option<String>,

        /// Base URL for the agent interface
        #[arg(long)]
        url: Option<String>,
    },
    /// List registered agents
    List,
    /// Show an agent's card
    Show {
        /// Agent name
        name: String,
    },
    /// Remove a registered agent
    Rm {
        /// Agent name
        name: String,
    },
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
        Some(Command::A2a { action }) => {
            let agents_dir = paths.home_dir.join("agents");
            match action {
                A2aAction::Register {
                    name,
                    card,
                    description,
                    version,
                    provider_org,
                    iss,
                    url,
                } => cli::a2a::run_register(
                    &agents_dir,
                    cli::a2a::RegisterOpts {
                        name,
                        card,
                        description,
                        version,
                        provider_org,
                        iss,
                        url,
                    },
                )?,
                A2aAction::List => cli::a2a::run_list(&agents_dir)?,
                A2aAction::Show { name } => cli::a2a::run_show(&agents_dir, &name)?,
                A2aAction::Rm { name } => cli::a2a::run_rm(&agents_dir, &name)?,
                A2aAction::Discover { url } => cli::a2a::run_discover(&url).await?,
                A2aAction::Send { url, message } => cli::a2a::run_send(&url, &message).await?,
                A2aAction::Status { url, task } => cli::a2a::run_status(&url, &task).await?,
                A2aAction::Cancel { url, task } => cli::a2a::run_cancel(&url, &task).await?,
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

    // ── First-run onboarding gate ───────────────────────────────
    // Auto-trigger cloud node onboarding on first serve/start when no
    // onboarding marker exists. Only runs in interactive terminals.
    let is_server_start =
        cli.command.is_none() || matches!(cli.command, Some(Command::Serve | Command::Start));
    let mut onboarding_ran = false;
    if is_server_start && cli::onboarding::should_onboard(&paths.home_dir) {
        let is_interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
        if is_interactive {
            onboarding_ran = true;
            match cli::onboarding::run_onboarding(&paths.home_dir) {
                Ok(cli::onboarding::OnboardingOutcome::CompletedCloud { rpc_url }) => {
                    // Write cloud provider default config and reload runtime.
                    if let Err(e) = write_cloud_provider_config(&paths, &rpc_url) {
                        eprintln!("  Warning: failed to write cloud config: {e}");
                    }
                    runtime = DefaultRuntime::load(paths.clone())
                        .unwrap_or_else(|_| DefaultRuntime::scaffold(paths.clone()));
                }
                Ok(cli::onboarding::OnboardingOutcome::CompletedByok) => {
                    // BYOK — no cloud config written.
                    println!();
                    println!("  To configure BYOK providers later, see:");
                    println!("    https://github.com/bitrouter/bitrouter#provider-configuration");
                    println!();
                }
                Ok(cli::onboarding::OnboardingOutcome::Deferred) => {
                    // Will re-prompt on next serve/start.
                }
                Err(e) => {
                    eprintln!("  Onboarding error: {e}");
                    eprintln!("  Continuing without cloud provider...");
                    eprintln!();
                }
            }
        }
    }

    // Auto-init: when launching in TUI mode with no providers, run the setup
    // wizard first so the user lands in a fully configured TUI.
    // Skip if onboarding already ran — the user chose their path.
    if use_tui && !onboarding_ran && !runtime.config.has_configured_providers() {
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
        Some(
            Command::Init
            | Command::A2a { .. }
            | Command::Account { .. }
            | Command::Keygen { .. }
            | Command::Keys { .. }
            | Command::Route { .. }
            | Command::Sudo { .. },
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

    // If cloud provider is already configured, skip.
    if existing.contains("bitrouter-cloud:") {
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
