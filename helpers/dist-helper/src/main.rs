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
    /// Generate the complete Registry v1 document and independent agent catalogs.
    Build {
        /// Fail if committed registry dist is stale instead of writing it.
        #[arg(long)]
        check: bool,
    },
    /// Audit the exact committed candidate against a fresh complete identity list.
    Audit {
        /// Write the machine-readable publication admission report.
        #[arg(long)]
        report: Option<PathBuf>,
        /// Previously promoted document for the same source; reject rollback.
        #[arg(long)]
        previous: Vec<PathBuf>,
    },
    /// Sync canonical model lifecycle and provider catalog feeds.
    Sync {
        /// Write canonical and provider YAML updates. Otherwise, print a dry-run report.
        #[arg(long)]
        write: bool,
        /// Write a machine-readable JSON sync and publication audit report.
        #[arg(long)]
        report: Option<PathBuf>,
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
    let root = workspace_root()?;
    match cli.command {
        Command::Package(options) => distribution::run(&root, &options),
        Command::GenerateSchema { check } => schema::generate(&root, check),
        Command::Registry { command } => match command {
            RegistryCommand::Validate => registry::validate(&root),
            RegistryCommand::Build { check } => registry::build(&root, check),
            RegistryCommand::Audit { report, previous } => {
                registry::audit(&root, report.as_deref(), &previous).await
            }
            RegistryCommand::Sync { write, report } => {
                registry::sync(&root, write, report.as_deref()).await
            }
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
