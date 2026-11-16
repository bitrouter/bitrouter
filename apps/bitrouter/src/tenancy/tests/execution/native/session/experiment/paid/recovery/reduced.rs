//! Old runtime produces obligations; independently built current profiles consume them.
use super::*;

async fn campaign(selector: &str, variable: &str) -> Result<()> {
    let binary =
        PathBuf::from(std::env::var_os(variable).with_context(|| format!("missing {variable}"))?);
    ensure!(binary.is_absolute() && binary.is_file());
    let mut child = tokio::process::Command::new(process::historical_producer()?)
        .args([
            "--exact",
            &format!("{PREFIX}reduced::{selector}"),
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(variable, binary)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut out = child
        .stdout
        .take()
        .context("reduced parent stdout")?
        .take(8193);
    let mut err = child
        .stderr
        .take()
        .context("reduced parent stderr")?
        .take(8193);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let completed = tokio::time::timeout(Duration::from_secs(180), async {
        let (_, _, status) = tokio::try_join!(
            out.read_to_end(&mut stdout),
            err.read_to_end(&mut stderr),
            child.wait()
        )?;
        ensure!(stdout.len() <= 8192 && stderr.len() <= 8192);
        ensure!(
            status.success(),
            "old producer/current reduced consumer campaign failed: {} {}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        ensure!(
            String::from_utf8_lossy(&stdout).contains("1 passed"),
            "zero selected tests"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await;
    if !matches!(completed, Ok(Ok(()))) {
        let _ = child.start_kill();
        tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
    }
    completed?
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_PAID_GATEWAY_LIBTEST"]
async fn gateway_refuses_stopped_native_paid_v4_with_clean_control() -> Result<()> {
    campaign(
        "gateway_refuses_stopped_native_paid_v4_with_clean_control",
        "BITROUTER_PAID_GATEWAY_LIBTEST",
    )
    .await
}
#[tokio::test]
#[ignore = "requires separately built BITROUTER_PAID_CORE_LIBTEST"]
async fn core_only_refuses_stopped_native_paid_v4_with_clean_control() -> Result<()> {
    campaign(
        "core_only_refuses_stopped_native_paid_v4_with_clean_control",
        "BITROUTER_PAID_CORE_LIBTEST",
    )
    .await
}
#[tokio::test]
#[ignore = "requires separately built BITROUTER_PAID_HARNESS_LIBTEST"]
async fn harness_only_refuses_stopped_native_paid_v4_with_clean_control() -> Result<()> {
    campaign(
        "harness_only_refuses_stopped_native_paid_v4_with_clean_control",
        "BITROUTER_PAID_HARNESS_LIBTEST",
    )
    .await
}
