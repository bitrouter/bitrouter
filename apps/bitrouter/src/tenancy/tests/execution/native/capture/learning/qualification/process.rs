//! One bounded Rust libtest child; errors always retain a failing result.
use super::*;
use std::{io::Write, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
pub(super) const ROOT: &str = "BITROUTER_PAID_GAP_ROOT";
pub(super) const MODE: &str = "BITROUTER_PAID_GAP_MODE";
pub(super) const SUBJECT: &str = "BITROUTER_PAID_GAP_SUBJECT";
pub(super) const JUDGE: &str = "BITROUTER_PAID_GAP_JUDGE";
pub(super) const WITNESS: &str = "paid-gap-real-owner-ready";
pub(super) struct Child {
    child: tokio::process::Child,
    output: BufReader<tokio::io::Take<tokio::process::ChildStdout>>,
    errors: AbortOnDropHandle<std::io::Result<Vec<u8>>>,
}
impl Child {
    pub(super) fn start(
        selector: &str,
        root: &Path,
        mode: &str,
        urls: Option<(&str, &str)>,
    ) -> Result<Self> {
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command
            .args([
                "--exact",
                selector,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROOT, root)
            .env(MODE, mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some((subject, judge)) = urls {
            command.env(SUBJECT, subject).env(JUDGE, judge);
        }
        let mut child = command.spawn()?;
        let output = BufReader::new(child.stdout.take().context("child stdout")?.take(16_385));
        let errors = child.stderr.take().context("child stderr")?;
        let errors = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut bytes = Vec::new();
            errors.take(16_385).read_to_end(&mut bytes).await?;
            Ok(bytes)
        }));
        Ok(Self {
            child,
            output,
            errors,
        })
    }
    pub(super) async fn ready(&mut self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(40), async {
            let mut total = 0usize;
            loop {
                let mut line = String::new();
                let read = self.output.read_line(&mut line).await?;
                total = total
                    .checked_add(read)
                    .context("child diagnostics overflow")?;
                ensure!(
                    read > 0 && total <= 16_384,
                    "child ended before bounded custody witness"
                );
                if line.trim() == WITNESS {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await?
    }
    pub(super) async fn finish(mut self, crash: bool, primary: Result<()>) -> Result<()> {
        let killed = if crash || primary.is_err() {
            self.child.start_kill().map_err(anyhow::Error::from)
        } else {
            Ok(())
        };
        drop(self.child.stdin.take());
        let status = match tokio::time::timeout(Duration::from_secs(6), self.child.wait()).await {
            Ok(status) => status.map_err(anyhow::Error::from),
            Err(error) => {
                let _kill = self.child.start_kill();
                let _reap = tokio::time::timeout(Duration::from_secs(6), self.child.wait()).await;
                Err(error.into())
            }
        };
        let errors = tokio::time::timeout(TIMEOUT, self.errors).await???;
        ensure!(errors.len() <= 16_384, "child stderr bound exceeded");
        primary.with_context(|| format!("child stderr: {}", String::from_utf8_lossy(&errors)))?;
        killed?;
        let status = status?;
        if crash {
            ensure!(!status.success(), "crash fixture exited gracefully");
        } else {
            ensure!(
                status.success(),
                "child failed: {}",
                String::from_utf8_lossy(&errors)
            );
        }
        Ok(())
    }
}
pub(super) async fn witness_and_wait() -> Result<()> {
    println!("\n{WITNESS}");
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(30), input.read_line(&mut line)).await??;
    Ok(())
}
