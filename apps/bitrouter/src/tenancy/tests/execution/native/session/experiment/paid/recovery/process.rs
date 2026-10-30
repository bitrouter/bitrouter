use super::*;

pub(super) struct Child {
    child: tokio::process::Child,
    output: BufReader<tokio::io::Take<tokio::process::ChildStdout>>,
    consumed: usize,
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
                &format!("{PREFIX}{selector}"),
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROOT, root)
            .env(MODE, mode)
            .env(
                KEY,
                base64::engine::general_purpose::STANDARD.encode([29u8; 32]),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some((subject, judge)) = urls {
            command.env(SUBJECT, subject).env(JUDGE, judge);
        }
        let mut child = command.spawn()?;
        let output = BufReader::new(child.stdout.take().context("paid child stdout")?.take(8193));
        let stderr = child.stderr.take().context("paid child stderr")?;
        let errors = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.take(8193).read_to_end(&mut bytes).await?;
            Ok(bytes)
        }));
        Ok(Self {
            child,
            output,
            consumed: 0,
            errors,
        })
    }
    pub(super) async fn line(&mut self, expected: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                let mut line = String::new();
                let read = self.output.read_line(&mut line).await?;
                self.consumed = self
                    .consumed
                    .checked_add(read)
                    .context("paid stdout overflow")?;
                ensure!(
                    read > 0 && self.consumed <= 8192,
                    "paid child ended before bounded witness {expected}"
                );
                if line.trim() == expected {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await?
    }
    // Fixed scheduling acknowledgement only: the child still owns and executes
    // the original SQL future and independently checks H/receipt evidence.
    pub(super) async fn advance(&mut self) -> Result<()> {
        let input = self.child.stdin.as_mut().context("paid child stdin")?;
        tokio::time::timeout(TIMEOUT, async {
            tokio::io::AsyncWriteExt::write_all(input, b"release-commit\n").await?;
            tokio::io::AsyncWriteExt::flush(input).await
        })
        .await??;
        Ok(())
    }

    pub(super) async fn finish(mut self, kill: bool, checked: Result<()>) -> Result<()> {
        let killed = if kill || checked.is_err() {
            self.child.start_kill().map_err(anyhow::Error::from)
        } else {
            Ok(())
        };
        drop(self.child.stdin.take());
        // Keep the SAME bounded reader after its ACK, draining concurrently with
        // the real wait; libtest tail/diagnostics cannot strand a live child.
        let tail = async {
            let mut tail = Vec::new();
            self.output.read_to_end(&mut tail).await?;
            Ok::<_, std::io::Error>(tail)
        };
        let waited = tokio::time::timeout(Duration::from_secs(45), async {
            tokio::join!(self.child.wait(), tail)
        })
        .await;
        let waited = match waited {
            Ok((status, tail)) => status
                .map_err(anyhow::Error::from)
                .and_then(|status| Ok((status, tail?))),
            Err(error) => {
                let kill_again = self.child.start_kill();
                let reap = tokio::time::timeout(Duration::from_secs(6), self.child.wait()).await;
                ensure!(reap.is_ok(), "paid child failed final reap: {kill_again:?}");
                Err(error.into())
            }
        };
        let errors = tokio::time::timeout(TIMEOUT, self.errors).await???;
        let (status, tail) = waited?;
        let stdout_bytes = self.consumed.checked_add(tail.len());
        let diagnostic = format!(
            "paid child diagnostics: status={status}, stdout_bytes={stdout_bytes:?}, stderr_bytes={}, stderr_prefix={}",
            errors.len(),
            stderr_prefix(&errors)
        );
        check_diagnostics(&errors, stdout_bytes, checked).context(diagnostic.clone())?;
        killed?;
        ensure!(status.success() != kill, "{diagnostic}");
        Ok(())
    }
}

// Keep the original witness failure AND the strict length failure. A child
// backtrace exceeding the unchanged 8192-byte cap must not erase its first error.
fn check_diagnostics(
    errors: &[u8],
    stdout_bytes: Option<usize>,
    checked: Result<()>,
) -> Result<()> {
    if errors.len() <= 8192 && stdout_bytes.is_some_and(|bytes| bytes <= 8192) {
        return checked;
    }
    match checked {
        Err(error) => Err(error.context("paid child diagnostics bound")),
        Ok(()) => anyhow::bail!("paid child diagnostics bound"),
    }
}

fn stderr_prefix(errors: &[u8]) -> String {
    // Escape control bytes and retain only a bounded leading diagnostic. The
    // overflow remains a failure; truncating this preview can never make it pass.
    let end = errors.len().min(1024);
    format!("{:?}", String::from_utf8_lossy(&errors[..end]))
}
