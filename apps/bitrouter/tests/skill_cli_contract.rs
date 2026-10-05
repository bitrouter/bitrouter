//! The shipped Agent Skill's local control path, without an MCP origin.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};

fn run_json(config: &Path, args: &[&str]) -> Result<serde_json::Value> {
    let output = Command::new(env!("CARGO_BIN_EXE_bro"))
        .args(args)
        .arg("--config")
        .arg(config)
        .output()
        .with_context(|| format!("running bro {}", args.join(" ")))?;
    ensure!(
        output.status.success(),
        "bro {} failed with {}: stderr={} stdout={}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("bro {} did not emit JSON", args.join(" ")))
}

#[test]
fn skill_can_inspect_status_models_and_route_through_cli_alone() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = directory.path().join("bitrouter.yaml");
    std::fs::write(
        &config,
        r#"
server:
  listen: 127.0.0.1:4356
  skip_auth: true
  control_socket: ./bitrouter.sock
providers:
  fixture:
    api_base: https://fixture.invalid/v1
    api_key: fixture-key
    models:
      - id: fixture-model
"#,
    )?;

    let skill = include_str!("../../../skills/bitrouter/SKILL.md");
    for command in ["bro status", "bro models", "bro route"] {
        ensure!(skill.contains(command), "shipped Skill omits `{command}`");
    }

    let status = run_json(&config, &["status"])?;
    ensure!(status["running"] == false, "unexpected status: {status}");

    let models = run_json(&config, &["models"])?;
    ensure!(
        models["models"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["id"] == "fixture-model")),
        "fixture model missing: {models}"
    );

    let route = run_json(&config, &["route", "fixture-model"])?;
    ensure!(
        route["effective_model"] == "fixture-model",
        "unexpected route: {route}"
    );
    Ok(())
}
