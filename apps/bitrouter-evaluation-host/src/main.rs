//! Explicit evaluation-format composition; stock `bro` stays generic.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::Result;
use bitrouter_sdk::extension::ExtensionApi;
use clap::Parser;

#[derive(Parser)]
#[command(name = "bro-evaluate")]
struct Args {
    /// BitRouter YAML configuration. TypeSafe still requires TYPESAFE_API_KEY.
    #[arg(short, long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let source = bitrouter::paths::resolve_config(Some(&args.config))?;
    bitrouter::host::serve_with_extensions(&source, |api: &mut ExtensionApi| {
        bitrouter_system_one_format::register(api)?;
        Ok(())
    })
    .await
}
