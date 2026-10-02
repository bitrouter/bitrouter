//! Run a foreground BitRouter host with one explicitly linked regex checker.
//! Usage: cargo run -p bitrouter --example native_regex_checker -- CONFIG RULES
//! Management commands target CONFIG or its control socket. To restart, launch
//! this custom binary again; official `bro restart` cannot retain its registrations.

use std::path::PathBuf;

use anyhow::{Context, Result};
use bitrouter_guardrails::{checker, config::InputGuardrailConfig};
use bitrouter_sdk::extension::ExtensionApi;

fn register(api: &mut ExtensionApi, rules: InputGuardrailConfig) -> Result<()> {
    Ok(api.request_check(
        "secret-check",
        "secret-rules-v1",
        checker::callback(rules.compile()?),
    )?)
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
    let source = bitrouter::paths::resolve_config(Some(&config_path))?;
    let rules: InputGuardrailConfig =
        serde_saphyr::from_str(&std::fs::read_to_string(rules_path)?)?;
    // This module owns the revision; do not read it back from config and
    // blindly echo it. Update it when the callback or loaded rule set changes.
    bitrouter::host::serve_with_extensions(&source, move |api| register(api, rules)).await
}
