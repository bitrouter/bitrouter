//! No builds or global environment mutation; each profile supplies its own binary.
use super::*;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::task::AbortOnDropHandle;

const PRODUCER: &str =
    "tenancy::tests::execution::native::capture::learning::startup::full_producer";
const CONSUMER: &str = "tenancy::tests::learning_startup::reduced_consumer";

enum Role<'a> {
    Producer(&'a str),
    Consumer(&'a str),
}

async fn child(binary: &Path, root: &Path, role: Role<'_>) -> Result<()> {
    let (selector, mode, profile) = match role {
        Role::Producer(mode) => (PRODUCER, mode, ""),
        Role::Consumer(profile) => (CONSUMER, "", profile),
    };
    let mut child = tokio::process::Command::new(binary)
        .args([
            "--exact",
            selector,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROOT, root)
        .env(MODE, mode)
        .env(PROFILE, profile)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().context("child stdout")?;
    let stderr = child.stderr.take().context("child stderr")?;
    let diagnostics = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.take(8193).read_to_end(&mut bytes).await?;
        Ok::<_, anyhow::Error>(bytes)
    }));
    let mut output = BufReader::new(stdout.take(8193));
    let mut total = 0;
    let checked: Result<()> = async {
        if !mode.is_empty() {
            tokio::time::timeout(Duration::from_secs(45), async {
                loop {
                    let mut line = String::new();
                    let bytes = output.read_line(&mut line).await?;
                    total += bytes;
                    ensure!(
                        bytes != 0 && total <= 8192,
                        "missing exact producer witness or excess diagnostics"
                    );
                    if line.trim() == WITNESS {
                        break Ok::<_, anyhow::Error>(());
                    }
                }
            })
            .await??;
        }
        if mode == "pending" {
            let path = root.join("pending.db");
            match ownership::acquire_path(&path, false) {
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) => {}
                Err(error) => {
                    return Err(error.context("failed to inspect pending producer file lease"));
                }
                Ok(_) => anyhow::bail!("pending producer lost physical file custody"),
            }
            child.start_kill()?;
        }
        Ok(())
    }
    .await;
    if checked.is_err() {
        let _kill = child.start_kill();
    }
    // Retain and drain this same capped reader through actual process reap.
    let mut tail = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(40), async {
        tokio::join!(child.wait(), output.read_to_end(&mut tail))
    })
    .await;
    if ended.is_err() {
        let _kill = child.start_kill();
        let _reap = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }
    let errors = tokio::time::timeout(Duration::from_secs(5), diagnostics).await???;
    checked.with_context(|| format!("{selector}: {}", String::from_utf8_lossy(&errors)))?;
    let (status, read) = ended?;
    read?;
    ensure!(total + tail.len() <= 8192 && errors.len() <= 8192);
    let status = status?;
    if mode == "pending" {
        // Prove deliberate crash, not an assertion failure with an arbitrary
        // nonzero status. The ACK above followed a real held judge request.
        use std::os::unix::process::ExitStatusExt;
        ensure!(
            status.signal() == Some(9),
            "producer was not killed after its witness"
        );
    } else {
        ensure!(
            status.success(),
            "{selector} failed: {}",
            String::from_utf8_lossy(&errors)
        );
    }
    drop(output);
    Ok(())
}

async fn campaign(profile: &str, variable: &str) -> Result<()> {
    let binary = std::path::PathBuf::from(
        std::env::var_os(variable).with_context(|| format!("missing {variable}"))?,
    );
    ensure!(binary.is_absolute() && binary.is_file());
    let root = tempfile::tempdir()?;
    let producer = std::env::current_exe()?;
    child(&producer, root.path(), Role::Producer("clean")).await?;
    child(&producer, root.path(), Role::Producer("pending")).await?;
    for name in ["clean", "pending"] {
        let path = root.path().join(format!("{name}.db"));
        ensure!(std::fs::metadata(&path)?.len() > 0);
        drop(ownership::acquire_path(&path, false)?);
    }
    child(&binary, root.path(), Role::Consumer(profile)).await?;
    // libtest zero matches also exits zero; only the real consumer can emit this.
    ensure!(
        std::fs::read(root.path().join("consumer-ack"))?
            == format!("paid-pending-refused:{profile}").as_bytes()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires BITROUTER_PAID_GATEWAY_LIBTEST (gateway without core/harness)"]
async fn gateway_refuses_pending_inactive_paid_draft() -> Result<()> {
    campaign("gateway", "BITROUTER_PAID_GATEWAY_LIBTEST").await
}
#[tokio::test]
#[ignore = "requires BITROUTER_PAID_CORE_LIBTEST (core without harness)"]
async fn core_only_refuses_pending_inactive_paid_draft() -> Result<()> {
    campaign("core-only", "BITROUTER_PAID_CORE_LIBTEST").await
}
#[tokio::test]
#[ignore = "requires BITROUTER_PAID_HARNESS_LIBTEST (harness without core)"]
async fn harness_only_refuses_pending_inactive_paid_draft() -> Result<()> {
    campaign("harness-only", "BITROUTER_PAID_HARNESS_LIBTEST").await
}
