use std::path::PathBuf;

use bitrouter_runtime::AppRuntime;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "bitrouter", version, about = "BitRouter CLI")]
struct Cli {
    #[arg(long, global = true, default_value = "bitrouter.toml")]
    config: PathBuf,

    /// Run server without the TUI (headless mode)
    #[arg(long)]
    headless: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show runtime status
    Status,
    /// Start as background daemon
    Start,
    /// Stop the daemon
    Stop,
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

    let runtime = match &cli.command {
        None | Some(Command::Status) => {
            AppRuntime::load(&cli.config).unwrap_or_else(|_| AppRuntime::scaffold(&cli.config))
        }
        _ => AppRuntime::load(&cli.config)?,
    };

    match cli.command {
        None => run_interactive(runtime, cli.headless).await?,
        Some(Command::Status) => {
            let status = runtime.status();
            println!("config: {}", status.config_file.display());
            println!("runtime: {}", status.runtime_dir.display());
            println!("listen: {}", status.listen_addr);
        }
        Some(Command::Start) => runtime.start().await?,
        Some(Command::Stop) => runtime.stop().await?,
        Some(Command::Restart) => runtime.restart().await?,
    }

    Ok(())
}

async fn run_interactive(
    runtime: AppRuntime,
    headless: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let status = runtime.status();

    if headless {
        runtime.serve().await?;
        return Ok(());
    }

    #[cfg(feature = "tui")]
    {
        let tui_config = bitrouter_tui::TuiConfig {
            listen_addr: status.listen_addr,
            providers: vec![], // TODO: populate from config
            route_count: 0,   // TODO: populate from routing table
        };

        let server_handle = tokio::spawn(async move {
            if let Err(e) = runtime.serve().await {
                tracing::error!("server error: {e}");
            }
        });

        bitrouter_tui::run(tui_config).await?;

        server_handle.abort();
    }

    #[cfg(not(feature = "tui"))]
    {
        let _ = status;
        runtime.serve().await?;
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
