//! The migration command exposes a reviewable candidate before changing config.

use anyhow::{Context, Result, ensure};
use std::path::Path;
use std::process::{Command, Output};

fn invoke(home: &Path, args: &[&str]) -> Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_bro"))
        .current_dir(home)
        .env("BITROUTER_HOME", home)
        .args(args)
        .output()
        .context("running bro config migration")
}

#[test]
fn cli_preview_apply_and_legacy_validation_guidance() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    let source = home.join("bitrouter.yaml");
    let raw = "inherit_defaults: false\npresets:\n  coding:\n    model: fixture:base\n    policy: null # no dynamic policy\n";
    std::fs::write(&source, raw)?;
    let validated = invoke(home, &["config", "validate"])?;
    ensure!(
        validated.status.success(),
        "validation failed: {}",
        String::from_utf8_lossy(&validated.stdout)
    );
    let validation: serde_json::Value = serde_json::from_slice(&validated.stdout)?;
    ensure!(validation["migration_hint"].is_string());
    ensure!(validation["routers"] == 0);
    let preview = invoke(
        home,
        &["config", "migrate-routers", "--candidate", "candidate.yaml"],
    )?;
    ensure!(
        preview.status.success(),
        "preview failed: {}",
        String::from_utf8_lossy(&preview.stdout)
    );
    let report: serde_json::Value = serde_json::from_slice(&preview.stdout)?;
    ensure!(report["applied"] == false);
    ensure!(std::fs::read_to_string(&source)? == raw);
    let digest = report["source_digest"]
        .as_str()
        .context("missing source digest")?;
    let invalid = invoke(home, &["config", "migrate-routers", "--apply"])?;
    ensure!(
        !invalid.status.success(),
        "apply accepted without reviewed candidate"
    );
    let applied = invoke(
        home,
        &[
            "config",
            "migrate-routers",
            "--candidate",
            "candidate.yaml",
            "--apply",
            "--source-digest",
            digest,
        ],
    )?;
    ensure!(
        applied.status.success(),
        "apply failed: {}",
        String::from_utf8_lossy(&applied.stdout)
    );
    let result: serde_json::Value = serde_json::from_slice(&applied.stdout)?;
    ensure!(result["applied"] == true && result["restart_required"] == true);
    let backup = result["backup"].as_str().context("missing backup")?;
    ensure!(std::fs::read_to_string(home.join(backup))? == raw);
    let migrated = bitrouter_sdk::config::parse(&std::fs::read_to_string(source)?)?;
    ensure!(migrated.presets.is_empty());
    ensure!(migrated.resolve_router("@coding")?.clean_model == "fixture:base");
    ensure!(migrated.resolve_router("bitrouter/coding")?.clean_model == "fixture:base");
    Ok(())
}
