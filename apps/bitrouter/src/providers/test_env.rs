//! Environment cases run the exact test in a child with caller-set variables.

use anyhow::{Context, Result};

pub(super) fn with_env(
    key: &str,
    value: Option<&str>,
    check: impl FnOnce() -> Result<()>,
) -> Result<()> {
    const CHILD_TEST: &str = "BITROUTER_PROVIDER_ENV_TEST";
    let thread = std::thread::current();
    let name = thread.name().context("test thread has no name")?;
    if std::env::var(CHILD_TEST).ok().as_deref() == Some(name) {
        return check();
    }
    let mut child = std::process::Command::new(std::env::current_exe()?);
    child
        .args(["--exact", name, "--nocapture"])
        .env(CHILD_TEST, name);
    match value {
        Some(value) => {
            child.env(key, value);
        }
        None => {
            child.env_remove(key);
        }
    }
    let output = child
        .output()
        .context("running provider environment case")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    anyhow::ensure!(
        output.status.success() && stdout.contains("running 1 test") && stdout.contains("1 passed"),
        "isolated provider case failed: {}\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
