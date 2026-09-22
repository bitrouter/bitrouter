//! Explicit TypeSafe native-extension composition; stock `bro` stays generic.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::{Context, Result};
use bitrouter_sdk::extension::ExtensionApi;
use bitrouter_sdk::server::RouterOptions;
use clap::Parser;

#[derive(Parser)]
#[command(name = "bro-typesafe")]
struct Args {
    /// BitRouter YAML configuration. Registry routes still require the
    /// corresponding provider credential (TYPESAFE_API_KEY for TypeSafe).
    #[arg(short, long)]
    config: PathBuf,
    /// Inference HTTP listener; defaults to the normal BitRouter port.
    #[arg(long)]
    listen: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut extensions = ExtensionApi::new();
    bitrouter_typesafe_extension::register(&mut extensions)
        .context("register TypeSafe System One format")?;
    let mut config = bitrouter_sdk::config::load(&args.config)
        .await
        .with_context(|| format!("load {}", args.config.display()))?;
    bitrouter::assemble::merge_registry_into_with_extensions(&mut config, &extensions).await;
    let assembled = bitrouter::assemble::build_app_with_extensions(
        &config,
        Some(&args.config),
        &extensions,
        None,
    )
    .await?;
    let extra = bitrouter::evaluation_http::router(&config, &assembled);
    let pipeline = assembled.evaluation_pipeline.clone();
    let listen = args.listen.unwrap_or_else(|| config.server.listen.clone());
    let options = RouterOptions {
        omit_v1_models: true,
        ..RouterOptions::default()
    }
    .with_router_wrapper(move |router| router.merge(extra.clone()));
    let served = assembled
        .app
        .serve_with_router_options_and_shutdown(&listen, options, shutdown_signal())
        .await;
    if let Some(pipeline) = pipeline {
        pipeline.drain().await;
    }
    served.context("serve TypeSafe evaluation host")?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
