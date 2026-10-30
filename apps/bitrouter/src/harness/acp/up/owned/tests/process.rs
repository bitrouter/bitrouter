//! Real OS group state, using the same absence probe as the production reaper.
use super::super::wait_for_group_exit;
use anyhow::{Context, Result, ensure};
use futures::FutureExt;
use rustix::process::{Pid, getpgid, test_kill_process_group};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn reaped_leader_with_live_descendant_is_not_group_absence() -> Result<()> {
    let program = PathBuf::from(
        std::env::var_os("BITROUTER_OWNED_CAPTURE_FIXTURE").context("prebuilt peer")?,
    );
    ensure!(program.is_absolute() && program.is_file());
    let root = tempfile::tempdir()?;
    let initialize_path = root.path().join("initialize.sock");
    let descendant_path = root.path().join("descendant.sock");
    let initialize = tokio::net::UnixListener::bind(&initialize_path)?;
    let descendants = tokio::net::UnixListener::bind(&descendant_path)?;
    let mut child = tokio::process::Command::new(program)
        .env_clear()
        .env("CAPTURE_FIXTURE_MARKER", root.path().join("child.json"))
        .env("CAPTURE_FIXTURE_INITIALIZE_GATE", &initialize_path)
        .env("CAPTURE_FIXTURE_DESCENDANT_GATE", &descendant_path)
        .env("CAPTURE_FIXTURE_EXIT_AT_INITIALIZE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let group = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(Pid::from_raw);
    let mut stderr = child.stderr.take().map(|mut stderr| {
        tokio::spawn(async move { tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await })
    });
    let mut initializer = None;
    let mut descendant = None;
    let mut reaped = false;
    let checked: Result<()> = async {
        let group = group.context("owned group ID")?;
        child
            .stdin
            .as_mut()
            .context("child input")?
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}\n")
            .await?;
        initializer = Some(tokio::time::timeout(TIMEOUT, initialize.accept()).await??.0);
        let peer = initializer.as_mut().context("initializer")?;
        let mut ready = [0; 5];
        tokio::time::timeout(TIMEOUT, peer.read_exact(&mut ready)).await??;
        ensure!(&ready == b"ready");
        descendant = Some(tokio::time::timeout(TIMEOUT, descendants.accept()).await??.0);
        let leaf = descendant.as_mut().context("descendant")?;
        let mut raw = [0; 4];
        tokio::time::timeout(TIMEOUT, leaf.read_exact(&mut raw)).await??;
        let pid = Pid::from_raw(i32::try_from(u32::from_be_bytes(raw))?)
            .context("descendant PID")?;
        ensure!(pid != group && getpgid(Some(pid))? == group);
        peer.write_all(&[1]).await?;
        let status = tokio::time::timeout(TIMEOUT, child.wait()).await??;
        reaped = true;
        ensure!(status.success(), "leader did not exit normally");
        // A response AFTER actual child.wait proves a live descendant; a PID
        // number, sleep, or leader-exit status alone cannot establish this.
        ping(leaf).await?;
        ensure!(getpgid(Some(pid))? == group);
        let work = wait_for_group_exit(|| test_kill_process_group(group));
        tokio::pin!(work);
        ensure!(work.as_mut().now_or_never().is_none());
        ping(leaf).await?;
        ensure!(work.as_mut().now_or_never().is_none());
        ensure!(
            !stderr.as_ref().context("stderr owner")?.is_finished(),
            "live descendant did not retain inherited stderr"
        );
        drop(descendant.take());
        tokio::time::timeout(TIMEOUT, &mut work).await??;
        ensure!(test_kill_process_group(group) == Err(rustix::io::Errno::SRCH));
        Ok(())
    }
    .await;
    // EOF belongs only to these test peers and is fallback containment on any
    // failed assertion. Never repeat killpg after the leader has been reaped.
    drop(descendant.take());
    drop(initializer.take());
    drop(child.stdin.take());
    let cleanup: Result<()> = async {
        if !reaped {
            match tokio::time::timeout(TIMEOUT, child.wait()).await {
                Ok(result) => {
                    result?;
                }
                Err(_) => {
                    child.start_kill()?;
                    tokio::time::timeout(TIMEOUT, child.wait()).await??;
                }
            }
        }
        if let Some(group) = group {
            tokio::time::timeout(
                TIMEOUT,
                wait_for_group_exit(|| test_kill_process_group(group)),
            )
            .await??;
        }
        Ok(())
    }
    .await;
    let drained: Result<()> = async {
        let task = stderr.as_mut().context("stderr task retained")?;
        tokio::time::timeout(TIMEOUT, task).await???;
        Ok(())
    }
    .await;
    let result = checked.and(cleanup).and(drained);
    if let Err(error) = result {
        let retained = root.keep();
        anyhow::bail!(
            "{error:#}; failed process fixture retained at {}",
            retained.display()
        );
    }
    Ok(())
}

async fn ping(peer: &mut tokio::net::UnixStream) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        peer.write_all(b"p").await?;
        let mut reply = [0];
        peer.read_exact(&mut reply).await?;
        ensure!(reply == *b"p");
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
