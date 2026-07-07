//! Internal helper for committed `dist/` artifacts.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod registry;
mod schema;

#[derive(Debug, Parser)]
#[command(name = "dist-helper")]
#[command(about = "Generate and check committed dist artifacts")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate the config JSON Schema.
    GenerateSchema {
        /// Fail if the committed schema is stale instead of writing it.
        #[arg(long)]
        check: bool,
    },
    /// Registry source-data and dist operations.
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
    },
    /// Check every committed dist artifact managed by this helper.
    Check,
}

#[derive(Debug, Subcommand)]
enum RegistryCommand {
    /// Validate `registry/` source YAML.
    Validate,
    /// Generate `dist/registry/{providers,models}.json`.
    Build {
        /// Fail if committed registry dist is stale instead of writing it.
        #[arg(long)]
        check: bool,
    },
    /// Keyless models.dev sync for `auto_sync: { feed: models_dev }` providers.
    Sync {
        /// Write provider YAML updates. Without this, print a dry-run report.
        #[arg(long)]
        write: bool,
    },
    /// Render the prompt used by the headless agentic registry sync step.
    AgenticPrompt,
    /// Check that an agentic sync made a narrow, reviewable registry diff.
    AgenticDiffCheck {
        /// Maximum deleted lines allowed in one provider YAML file.
        #[arg(long, default_value_t = 80)]
        max_provider_deletions: usize,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("dist-helper: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let root = workspace_root();
    match cli.command {
        Command::GenerateSchema { check } => schema::generate(&root, check),
        Command::Registry { command } => match command {
            RegistryCommand::Validate => registry::validate(&root),
            RegistryCommand::Build { check } => registry::build(&root, check),
            RegistryCommand::Sync { write } => registry::sync(&root, write).await,
            RegistryCommand::AgenticPrompt => {
                print!("{}", registry::agentic_prompt(&root)?);
                Ok(())
            }
            RegistryCommand::AgenticDiffCheck {
                max_provider_deletions,
            } => registry::agentic_diff_check(&root, max_provider_deletions),
        },
        Command::Check => {
            schema::generate(&root, true)?;
            registry::build(&root, true)
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("helpers/dist-helper lives two levels below the workspace root")
        .to_path_buf()
}
