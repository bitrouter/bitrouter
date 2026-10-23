//! Internal helper for committed `dist/` artifacts and profiled packages.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod chart;
mod distribution;
mod registry;
mod schema;

#[derive(Debug, Parser)]
#[command(name = "dist-helper")]
#[command(about = "Generate and check dist artifacts and build profiled packages")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build and package a distinct gateway or full artifact for manual use.
    Package(distribution::PackageOptions),
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
    /// Render the README registry chart SVGs (light + dark).
    Chart {
        /// Directory to write `registry-by-lab{,-dark}.svg` into.
        #[arg(long, default_value = "dist/charts")]
        out: PathBuf,
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
    /// Sync canonical model lifecycle and provider catalog feeds.
    Sync {
        /// Write canonical and provider YAML updates. Otherwise, print a dry-run report.
        #[arg(long)]
        write: bool,
        /// Write a Markdown summary suitable for the registry sync PR body.
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// Render the prompt used by the headless agentic registry sync step.
    AgenticPrompt,
    /// Check that an agentic sync touched only registry source files.
    AgenticDiffCheck,
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
    let root = workspace_root()?;
    match cli.command {
        Command::Package(options) => distribution::run(&root, &options),
        Command::GenerateSchema { check } => schema::generate(&root, check),
        Command::Registry { command } => match command {
            RegistryCommand::Validate => registry::validate(&root),
            RegistryCommand::Build { check } => registry::build(&root, check),
            RegistryCommand::Sync { write, report } => {
                registry::sync(&root, write, report.as_deref()).await
            }
            RegistryCommand::AgenticPrompt => {
                print!("{}", registry::agentic_prompt(&root)?);
                Ok(())
            }
            RegistryCommand::AgenticDiffCheck => registry::agentic_diff_check(&root),
        },
        Command::Chart { out } => chart::generate(&root, &root.join(out)),
        Command::Check => {
            schema::generate(&root, true)?;
            registry::build(&root, true)
        }
    }
}

fn workspace_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .context("helpers/dist-helper must live two levels below the workspace root")
}
