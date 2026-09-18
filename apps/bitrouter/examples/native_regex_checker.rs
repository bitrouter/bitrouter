//! Run a custom HTTP model gateway with one explicitly linked regex checker.
//! Usage: cargo run -p bitrouter --example native_regex_checker -- CONFIG RULES
//! This minimal embedding does not start the bro daemon management socket.

use std::path::PathBuf;

use anyhow::{Context, Result};
use bitrouter::extension::ExtensionApi;
use bitrouter_guardrails::{checker, config::InputGuardrailConfig};
use bitrouter_sdk::config;
use bitrouter_sdk::server::{AppState, build_router};

fn register(api: &mut ExtensionApi, rules: InputGuardrailConfig) -> Result<()> {
    api.request_check(
        "secret-check",
        "secret-rules-v1",
        checker::callback(rules.compile()?),
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let config_path = PathBuf::from(args.next().context("expected CONFIG path")?);
    let rules_path = PathBuf::from(args.next().context("expected RULES path")?);
    anyhow::ensure!(
        args.next().is_none(),
        "expected only CONFIG and RULES paths"
    );
    let config = config::parse(&std::fs::read_to_string(&config_path)?)?;
    let rules: InputGuardrailConfig =
        serde_saphyr::from_str(&std::fs::read_to_string(rules_path)?)?;
    // This module owns the revision; do not read it back from config and
    // blindly echo it. Update it when the callback or loaded rule set changes.
    let assembled =
        bitrouter::assemble::build_app_with_extensions(&config, Some(&config_path), |api| {
            register(api, rules)
        })
        .await?;
    let router = build_router(AppState {
        language_model: assembled
            .app
            .language_model()
            .context("no pipeline")?
            .clone(),
        mcp: assembled.app.mcp().cloned(),
        skip_auth: assembled.app.skip_auth(),
        metrics_renderer: assembled.app.metrics_renderer().cloned(),
        prompt_transforms: assembled.app.prompt_transforms().to_vec(),
    });
    // An embedding owns its listener and lifecycle. Use an explicit loopback
    // server.listen in the example config for local development.
    let listener = tokio::net::TcpListener::bind(&config.server.listen).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
