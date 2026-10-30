//! Actual process containment is separate from production cleanup evidence.
use super::super::super::super::faults as capture_faults;
use super::*;
use std::process::Stdio;

pub(super) async fn isolated(mode: &str) -> Result<()> {
    executable()?;
    let root = tempfile::tempdir()?;
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            CHILD,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROOT, root.path())
        .env(MODE, mode)
        .env("RUST_BACKTRACE", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut output = child
        .stdout
        .take()
        .map(|stdout| BufReader::new(stdout.take(8193)));
    let errors = child
        .stderr
        .take()
        .map(|stderr| AbortOnDropHandle::new(tokio::spawn(capture_faults::diagnostics(stderr))));
    let mut read = 0_usize;
    let mut database = None;
    let checked = async {
        let output = output.as_mut().context("same retained stdout reader")?;
        ensure!(errors.is_some(), "stderr missing");
        tokio::time::timeout(Duration::from_secs(50), async {
            loop {
                let mut line = String::new();
                let n = output.read_line(&mut line).await?;
                ensure!(n > 0, "{mode}: child ended before original-owner witness");
                read = read.checked_add(n).context("stdout count overflow")?;
                ensure!(read <= 8192, "child stdout bound");
                if line.trim_end() == READY {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await??;
        let path = database_path(root.path()).await?;
        let conflict = crate::tenancy::ownership::acquire_path(&path, false)
            .err()
            .context("ordinary handle drop released original fenced host owner")?;
        ensure!(matches!(
            conflict.downcast_ref::<std::fs::TryLockError>(),
            Some(std::fs::TryLockError::WouldBlock)
        ));
        database = Some(path);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    // EOF contains the runtime, never acknowledges its fenced host Registry.
    // Attempt ALL cleanup paths even when the witness or a prior reap fails.
    drop(child.stdin.take());
    let runtime = capture_faults::reap_runtime(&mut child).await;
    let peer = capture_faults::cleanup_peer(&root.path().join("child.json")).await;
    let stdout = async {
        let Some(output) = output.as_mut() else {
            anyhow::bail!("stdout missing");
        };
        let mut tail = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), output.read_to_end(&mut tail)).await??;
        ensure!(
            read.checked_add(tail.len()).is_some_and(|n| n <= 8192),
            "full child stdout bound"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stderr = async {
        let errors = errors.context("stderr reader missing")?;
        tokio::time::timeout(Duration::from_secs(5), errors).await??
    }
    .await;
    let released = async {
        let path = match database {
            Some(path) => path,
            None => database_path(root.path()).await?,
        };
        crate::tenancy::ownership::acquire_path(&path, false)
            .map(drop)
            .context("original lease not released after isolated process containment")
    }
    .await;
    let mut failures = [checked, runtime, peer, stdout, released]
        .into_iter()
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
    let diagnostics = match stderr {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => {
            failures.push(format!("{error:#}"));
            String::new()
        }
    };
    if !failures.is_empty() {
        let retained = root.keep();
        anyhow::bail!(
            "{mode}: {}; retained {}; child stderr: {diagnostics}",
            failures.join("; "),
            retained.display()
        );
    }
    Ok(())
}

async fn database_path(root: &Path) -> Result<PathBuf> {
    let descriptor = root.join("database.txt");
    ensure!(tokio::fs::metadata(&descriptor).await?.len() <= 4096);
    let bytes = tokio::fs::read(&descriptor).await?;
    ensure!(bytes.len() <= 4096);
    let path = PathBuf::from(String::from_utf8(bytes)?).canonicalize()?;
    ensure!(
        path.starts_with(root.canonicalize()?)
            && path.file_name().is_some_and(|name| name == "block.sqlite")
    );
    let metadata = tokio::fs::metadata(&path).await?;
    ensure!(metadata.is_file() && metadata.len() > 0);
    Ok(path)
}
