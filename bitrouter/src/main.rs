use std::path::PathBuf;

use bitrouter_runtime::{AppRuntime, PathOverrides, resolve_home};
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
    let use_tui = cli.command.is_none() && !cli.headless;

    // Skip tracing init when TUI owns the terminal — logs corrupt the alternate screen
    if !use_tui {
        init_tracing();
    }

    // Resolve home directory + apply any per-path overrides
    let paths = resolve_home(cli.home_dir.as_deref());
    let overrides = PathOverrides {
        config_file: cli.config_file,
        env_file: cli.env_file,
        runtime_dir: cli.run_dir,
        log_dir: cli.logs_dir,
    };
    let paths = overrides.apply(paths);

    let runtime: DefaultRuntime =
        DefaultRuntime::load(paths.clone()).unwrap_or_else(|_| DefaultRuntime::scaffold(paths));

    match cli.command {
        None => run_default(runtime, cli.headless).await?,
        Some(Command::Serve) => {
            let model_router = bitrouter_runtime::Router::new(
                reqwest::Client::new(),
                runtime.config().providers.clone(),
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
    }

    Ok(())
}

async fn run_default(
    runtime: DefaultRuntime,
    headless: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let status = runtime.status();

    let model_router =
        bitrouter_runtime::Router::new(reqwest::Client::new(), runtime.config().providers.clone());

    if headless {
        runtime.serve(model_router).await?;
        return Ok(());
    }

    #[cfg(feature = "tui")]
    {
        let tui_config = bitrouter_tui::TuiConfig {
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
            result = bitrouter_tui::run(tui_config) => {
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
