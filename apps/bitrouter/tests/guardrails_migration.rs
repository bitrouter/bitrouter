//! Removed default-host guardrails must fail activation, never become ignored.

use anyhow::{Context, Result, ensure};
use bitrouter::daemon::DaemonReloader;
use bitrouter::paths::ConfigSource;
use bitrouter::reload::{AppReloader, ReloadParticipant, ReloadParticipantOutcome, ReloadSource};
use bitrouter_sdk::config;
use serde_json::{Value, json};
use std::process::Command;

fn configuration(legacy: Value) -> Value {
    json!({
        "inherit_defaults": false,
        "database": {"url": "sqlite://should-not-exist.db?mode=rwc"},
        "plugins": {"bitrouter-guardrails": legacy},
        "checkers": {"replacement": {
            "endpoint": "http://127.0.0.1:1/check", "contract_version": 1
        }},
        "routers": {"coding": {
            "selection": {"kind": "model", "model": "fixture:model"},
            "checks": {"request": [{"checker": "replacement"}]}
        }}
    })
}

fn verify_diagnostic(message: &str) -> Result<()> {
    for required in [
        "plugins.bitrouter-guardrails",
        "requires migration",
        "input",
        "global",
        "output",
    ] {
        ensure!(message.contains(required), "missing {required}: {message}");
    }
    Ok(())
}

#[tokio::test]
async fn host_and_saved_baseline_reject_every_legacy_value_before_side_effects() -> Result<()> {
    for legacy in [
        Value::Null,
        json!({}),
        json!({"custom_patterns": []}),
        json!({"custom_patterns": [{"name": "old", "pattern": "secret", "action": "block"}]}),
        json!({"custom_patterns": [{"name": "old", "pattern": "secret", "action": "redact"}]}),
    ] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bitrouter.yaml");
        let raw = serde_json::to_string(&configuration(legacy))?;
        // The SDK remains usable by custom hosts which explicitly install hooks.
        let config = config::parse(&raw)?;
        let error = match bitrouter::build_app(&config).await {
            Err(error) => error,
            Ok(_) => anyhow::bail!("legacy plugin silently activated"),
        };
        verify_diagnostic(&error.to_string())?;
        std::fs::write(&path, raw)?;
        let error = match bitrouter::reload::load_configuration_baseline(
            &bitrouter::paths::ConfigSource::File(path),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => anyhow::bail!("legacy saved baseline treated as valid"),
        };
        verify_diagnostic(&error.to_string())?;
    }
    Ok(())
}

#[tokio::test]
async fn live_reload_reports_a_safe_guardrails_migration_diagnostic() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("bitrouter.yaml");
    std::fs::write(
        &path,
        serde_json::to_string(&json!({
            "inherit_defaults": false,
            "database": {"url": "sqlite::memory:"}
        }))?,
    )?;
    let baseline =
        bitrouter::reload::load_configuration_baseline(&ConfigSource::File(path.clone())).await?;
    let assembled = bitrouter::build_app_with_path(baseline.config(), Some(&path)).await?;
    let reloader = AppReloader::new(
        assembled.policy_store,
        assembled.routing_table,
        assembled.upstream_executor,
        ReloadSource::File(path.clone()),
    )
    .with_startup_configuration(baseline);

    let private_name = "private-rule-name-must-not-leak";
    let private_pattern = "private-pattern-must-not-leak";
    std::fs::write(
        &path,
        serde_json::to_string(&configuration(json!({
            "custom_patterns": [{
                "name": private_name,
                "pattern": private_pattern,
                "action": "block"
            }]
        })))?,
    )?;

    ensure!(reloader.reload().await.is_err(), "legacy reload succeeded");
    let report = reloader
        .reload_state()
        .and_then(|state| state.last_outcome)
        .ok_or_else(|| anyhow::anyhow!("reload report is unavailable"))?;
    let routing = report
        .participants
        .iter()
        .find(|entry| entry.participant == ReloadParticipant::RoutingTable)
        .ok_or_else(|| anyhow::anyhow!("routing-table report is unavailable"))?;
    ensure!(
        routing.outcome == ReloadParticipantOutcome::Failed,
        "routing-table preparation did not fail: {routing:?}"
    );
    let failure = routing
        .error
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("routing-table failure is unavailable"))?;
    ensure!(
        failure.code == "guardrails_migration_required",
        "unexpected reload failure code: {}",
        failure.code
    );
    ensure!(
        failure.message
            == "saved configuration contains plugins.bitrouter-guardrails; migration is required before reload because router-bound request checkers cover input only and do not replace global or output protection",
        "unexpected reload failure message: {}",
        failure.message
    );
    let external_report = serde_json::to_string(&report)?;
    for private_value in [private_name, private_pattern, "should-not-exist.db"] {
        ensure!(
            !external_report.contains(private_value),
            "reload report exposed private configuration"
        );
    }
    Ok(())
}

#[test]
fn cli_validation_and_serve_block_legacy_without_rewriting_or_creating_database() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("bitrouter.yaml");
    let raw = serde_json::to_string(&configuration(json!({})))?;
    std::fs::write(&path, &raw)?;
    for args in [["config", "validate"].as_slice(), ["serve"].as_slice()] {
        let output = Command::new(env!("CARGO_BIN_EXE_bro"))
            .current_dir(directory.path())
            .env("BITROUTER_HOME", directory.path())
            .args(args)
            .output()
            .context("running legacy migration CLI check")?;
        ensure!(
            !output.status.success(),
            "legacy command succeeded: {args:?}"
        );
        let message = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        verify_diagnostic(&message)?;
        ensure!(
            std::fs::read_to_string(&path)? == raw,
            "config was rewritten"
        );
        ensure!(
            !directory.path().join("should-not-exist.db").exists(),
            "database was opened before validation"
        );
    }
    Ok(())
}
