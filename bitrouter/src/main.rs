use std::path::PathBuf;

use bitrouter_runtime::AppRuntime;
use clap::{Parser, Subcommand};

type DefaultRuntime = AppRuntime<bitrouter_config::ConfigRoutingTable>;

#[derive(Debug, Parser)]
#[command(name = "bitrouter", version, about = "BitRouter CLI")]
struct Cli {
    #[arg(long, global = true, default_value = "bitrouter.yaml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve,
    Start,
    Stop,
    Status,
    Restart,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    init_tracing();

    let runtime: DefaultRuntime = match cli.command {
        Command::Serve => DefaultRuntime::scaffold(cli.config),
        _ => DefaultRuntime::load(&cli.config)
            .unwrap_or_else(|_| DefaultRuntime::scaffold(cli.config)),
    };

    match cli.command {
        Command::Serve => runtime.serve(bitrouter_runtime::server::StubModelRouter).await?,
        Command::Start => runtime.start().await?,
        Command::Stop => runtime.stop().await?,
        Command::Status => {
            let status = runtime.status();
            println!("config:    {}", status.config_file.display());
            println!("runtime:   {}", status.runtime_dir.display());
            println!("listen:    {}", status.listen_addr);
            println!("providers: {}", status.providers.join(", "));
            if !status.models.is_empty() {
                println!("models:    {}", status.models.join(", "));
            }
        }
        Command::Restart => runtime.restart().await?,
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
