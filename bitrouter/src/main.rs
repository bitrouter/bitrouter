mod init;
mod runtime;
#[cfg(feature = "tui")]
mod tui;

use std::path::PathBuf;

use crate::runtime::{AppRuntime, PathOverrides, resolve_home};
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
        Some(Command::Init) => unreachable!(),
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
