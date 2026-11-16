//! Genuine predecessor terminal history survives two current bootstraps unchanged.
use super::*;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn native_paid_revision_budget_survives_rotation_reactivation_and_two_revision_bootstrap()
-> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(180), async {
        let root = tempfile::tempdir()?;
        let mut child = tokio::process::Command::new(recovery::process::historical_producer()?)
            .args(["--exact", "tenancy::tests::execution::native::session::experiment::paid::revision_budget::native_paid_revision_budget_survives_rotation_reactivation_and_two_revision_bootstrap", "--nocapture", "--test-threads=1"])
            .env("BITROUTER_PRE_M0_TERMINAL_ROOT", root.path())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn()?;
        let mut out = child.stdout.take().context("predecessor stdout")?.take(8193);
        let mut err = child.stderr.take().context("predecessor stderr")?.take(8193);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let (_, _, status) = tokio::try_join!(out.read_to_end(&mut stdout), err.read_to_end(&mut stderr), child.wait())?;
        ensure!(stdout.len() <= 8192 && stderr.len() <= 8192, "predecessor diagnostics bound");
        ensure!(status.success(), "terminal history producer failed: {}", String::from_utf8_lossy(&stderr));
        ensure!(String::from_utf8_lossy(&stdout).contains("pre-m0-terminal-history-exported"), "missing genuine history export acknowledgement");
        recovery::terminal::reopen_twice(root.path()).await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}
