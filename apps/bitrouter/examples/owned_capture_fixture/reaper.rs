//! Bounded process-group peer for the owned reaper's real-process tests.
use anyhow::{Context, Result, ensure};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) async fn leaf() -> Result<bool> {
    if std::env::var_os("CAPTURE_FIXTURE_REAPER_LEAF").is_none() {
        return Ok(false);
    }
    let socket = std::env::var("CAPTURE_FIXTURE_DESCENDANT_GATE")?;
    tokio::time::timeout(Duration::from_secs(40), async {
        let mut peer = tokio::net::UnixStream::connect(socket).await?;
        peer.write_all(&std::process::id().to_be_bytes()).await?;
        let mut byte = [0];
        loop {
            if peer.read(&mut byte).await? == 0 {
                return Ok::<_, anyhow::Error>(());
            }
            ensure!(byte == *b"p", "unexpected descendant test request");
            peer.write_all(b"p").await?;
        }
    })
    .await
    .context("descendant fixture lifetime exceeded")??;
    Ok(true)
}

pub(super) fn spawn_descendant() -> Result<Option<std::process::Child>> {
    let Some(socket) = std::env::var_os("CAPTURE_FIXTURE_DESCENDANT_GATE") else {
        return Ok(None);
    };
    // Inherit the actual ACP leader's process group and stderr. This retained
    // test socket supplies fallback EOF containment; no second group signal is
    // sent after the leader is reaped. std::process::Child drop does not kill.
    // https://doc.rust-lang.org/std/process/struct.Child.html
    let child = std::process::Command::new(std::env::current_exe()?)
        .env_clear()
        .env("CAPTURE_FIXTURE_REAPER_LEAF", "1")
        .env("CAPTURE_FIXTURE_DESCENDANT_GATE", socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(Some(child))
}
