//! End-to-end coverage for the canonical CLI name and compatibility alias.

use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result, ensure};

fn run(binary: &Path, args: &[&str], home: &Path) -> Result<Output> {
    Command::new(binary)
        .args(args)
        .env("BITROUTER_HOME", home)
        .env("NO_COLOR", "1")
        .output()
        .with_context(|| format!("running {}", binary.display()))
}

fn create_alias(binary: &Path, alias: &Path) -> Result<()> {
    #[cfg(unix)]
    std::os::unix::fs::symlink(binary, alias)?;
    #[cfg(windows)]
    let _ = std::fs::copy(binary, alias)?;
    Ok(())
}

#[test]
fn canonical_help_is_bro_and_has_no_alias_warning() -> Result<()> {
    let home = tempfile::tempdir()?;
    let binary = Path::new(env!("CARGO_BIN_EXE_bro"));
    let output = run(binary, &["--help"], home.path())?;
    ensure!(output.status.success(), "bro --help failed");
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    ensure!(stdout.contains("Usage: bro"), "unexpected help: {stdout}");
    ensure!(
        !stderr.contains("is now `bro`"),
        "canonical invocation warned: {stderr}"
    );
    Ok(())
}

#[test]
fn alias_preserves_help_and_structured_stdout() -> Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(home.path().join("bitrouter.yaml"), "{}\n")?;
    let binary = Path::new(env!("CARGO_BIN_EXE_bro"));
    let alias_name = if cfg!(windows) {
        "bitrouter.exe"
    } else {
        "bitrouter"
    };
    let alias = home.path().join(alias_name);
    create_alias(binary, &alias)?;

    for (args, expected_usage) in [
        (&["--help"][..], "Usage: bitrouter"),
        (
            &["launch", "--help"][..],
            "bitrouter launch <AGENT> [OPTIONS] [-- <AGENT_ARGS>...]",
        ),
        (
            &["acp", "serve", "--help"][..],
            "bitrouter acp serve <AGENT> [OPTIONS]",
        ),
    ] {
        let output = run(&alias, args, home.path())?;
        ensure!(output.status.success(), "alias help failed for {args:?}");
        let stdout = String::from_utf8(output.stdout)?;
        let stderr = String::from_utf8(output.stderr)?;
        ensure!(
            stdout.contains(expected_usage),
            "missing {expected_usage:?} in {stdout}"
        );
        ensure!(
            stderr.contains("note: `bitrouter` is now `bro`. This alias is removed in 1.0.0."),
            "missing alias warning in {stderr}"
        );
    }

    let output = run(&alias, &["status"], home.path())?;
    ensure!(output.status.success(), "alias status failed");
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .context("alias status stdout was not one JSON value")?;
    let stderr = String::from_utf8(output.stderr)?;
    ensure!(
        stderr.contains("note: `bitrouter` is now `bro`. This alias is removed in 1.0.0."),
        "missing alias warning in {stderr}"
    );
    Ok(())
}
