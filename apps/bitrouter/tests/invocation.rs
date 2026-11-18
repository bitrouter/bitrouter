//! Process coverage for the service's canonical executable and structured output.

use std::process::{Command, Output};

use anyhow::{Context, Result, ensure};

fn run(args: &[&str], home: &std::path::Path) -> Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_bitrouter"))
        .args(args)
        .current_dir(home)
        .env("BITROUTER_HOME", home)
        .env("NO_COLOR", "1")
        .output()
        .context("running the gateway CLI")
}

#[test]
fn gateway_help_and_status_use_the_service_identity() -> Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(home.path().join("bitrouter.yaml"), "{}\n")?;
    for (args, expected) in [
        (&["--help"][..], "Usage: bitrouter"),
        (&["serve", "--help"][..], "bitrouter serve"),
        (&["tenancy", "--help"][..], "bitrouter tenancy"),
    ] {
        let output = run(args, home.path())?;
        ensure!(output.status.success(), "help failed for {args:?}");
        let stdout = String::from_utf8(output.stdout)?;
        ensure!(stdout.contains(expected), "unexpected help: {stdout}");
        ensure!(output.stderr.is_empty(), "help emitted a diagnostic");
    }

    let status = run(&["status"], home.path())?;
    ensure!(status.status.success(), "gateway status failed");
    let report: serde_json::Value = serde_json::from_slice(&status.stdout)?;
    ensure!(
        report["running"] == false,
        "unexpected stopped status: {report}"
    );
    Ok(())
}
