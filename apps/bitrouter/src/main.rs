//! `bitrouter` CLI entry point — a thin shell over the `bitrouter` lib.
//!
//! v1.0 subcommands: `serve`, `init`, `key generate`, `models`. The lib/bin
//! split keeps assembly + management logic reusable (007). The full v0
//! subcommand surface (`status` / `wallet` / `agents` / `login` / TUI …) slots
//! into this same `Command` enum as it is migrated.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use bitrouter::commands;
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
    /// Load a config, run migrations, and serve the HTTP API.
    Serve {
        /// Path to `bitrouter.yaml`.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
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
    /// List every routable model for a config.
    Models {
        /// Path to `bitrouter.yaml`.
        #[arg(short, long, default_value = "bitrouter.yaml")]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum KeyAction {
    /// Mint a new `brvk_` virtual key for a user.
    Generate {
        /// The owning user id.
        #[arg(short, long)]
        user: String,
        /// Database URL.
        #[arg(short, long, default_value = "sqlite://./bitrouter.db")]
        db: String,
        /// Funding model for the key (`credits` / `mpp` / `byok` / `none`).
        #[arg(short, long, default_value = "credits")]
        payment_method: String,
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
        Command::Init { config } => init(&config).await,
        Command::Key { action } => key(action).await,
        Command::Models { config } => models(&config).await,
    }
}

async fn serve(config_path: &std::path::Path) -> Result<()> {
    let cfg = config::load(config_path)
        .await
        .with_context(|| format!("loading {}", config_path.display()))?;
    let listen = cfg.server.listen.clone();
    let assembled = bitrouter::build_app(&cfg).await?;
    println!("bitrouter {} — serving on {listen}", bitrouter::VERSION);
    assembled
        .app
        .serve(&listen)
        .await
        .context("serving the HTTP API")?;
    Ok(())
}

async fn init(config_path: &std::path::Path) -> Result<()> {
    commands::init(config_path).await?;
    println!("wrote starter config to {}", config_path.display());
    println!("  (skip_auth is on — credential-less local requests are admitted)");
    Ok(())
}

async fn key(action: KeyAction) -> Result<()> {
    match action {
        KeyAction::Generate {
            user,
            db,
            payment_method,
        } => {
            let pm = match payment_method.as_str() {
                "credits" => PaymentMethod::Credits,
                "mpp" => PaymentMethod::Mpp,
                "byok" => PaymentMethod::Byok,
                "none" => PaymentMethod::None,
                other => anyhow::bail!("unknown payment method '{other}'"),
            };
            let key = commands::key_generate(&db, &user, pm).await?;
            println!("created virtual key {} for user '{user}'", key.id);
            println!();
            println!("  {}", key.secret);
            println!();
            println!("This secret is shown ONCE — only its SHA-256 hash is stored.");
            Ok(())
        }
    }
}

async fn models(config_path: &std::path::Path) -> Result<()> {
    let cfg = config::load(config_path)
        .await
        .with_context(|| format!("loading {}", config_path.display()))?;
    let models = commands::list_models(&cfg).await?;
    if models.is_empty() {
        println!(
            "(no routable models — configure providers in {})",
            config_path.display()
        );
    }
    for (id, providers) in models {
        println!("{id}\t{}", providers.join(", "));
    }
    Ok(())
}
