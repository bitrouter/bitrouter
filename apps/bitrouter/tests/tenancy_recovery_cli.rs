//! Actual executable argument/response boundary; real provisioning lives in the
//! private lib-test consumer so it can reuse the existing connector and Service.
#![cfg(unix)]
use anyhow::{Context, Result, ensure};
use std::{
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn recover_requires_explicit_private_plan_without_loading_ambient_configuration() -> Result<()>
{
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    std::fs::write(
        directory.path().join("bitrouter.yaml"),
        "invalid: [AMBIENT_SECRET_SENTINEL",
    )?;
    let plan = directory.path().join("bad-plan.json");
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&plan)?;
    file.write_all(b"{\"format\":\"not-recovery\"}")?;
    file.sync_all()?;
    drop(file);
    let report = directory.path().join("report");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"))
        .args(["--json", "tenancy", "offline", "recover", "--plan"])
        .arg(&plan)
        .arg("--report")
        .arg(&report)
        .current_dir(directory.path())
        .env("BITROUTER_HOME", directory.path())
        .env("RUST_LOG", "error")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child.stdout.take().context("stdout")?;
    let mut stderr = child.stderr.take().context("stderr")?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let waited = tokio::time::timeout(Duration::from_secs(15), async {
        let (_, _, status) = tokio::try_join!(
            async {
                (&mut stdout).take(65537).read_to_end(&mut out).await?;
                ensure!(out.len() <= 65536, "stdout bound");
                Ok::<_, anyhow::Error>(())
            },
            async {
                (&mut stderr).take(65537).read_to_end(&mut err).await?;
                ensure!(err.len() <= 65536, "stderr bound");
                Ok::<_, anyhow::Error>(())
            },
            async { child.wait().await.map_err(anyhow::Error::from) }
        )?;
        Ok::<_, anyhow::Error>(status)
    })
    .await;
    if !matches!(waited, Ok(Ok(_))) {
        let _ = child.start_kill();
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .context("child reap deadline")??;
    }
    let status = waited.context("CLI deadline")??;
    ensure!(
        status.code() == Some(1),
        "unexpected recovery rejection status"
    );
    let value: serde_json::Value = serde_json::from_slice(&out)?;
    ensure!(
        value["error"]["kind"] == "bad_request"
            && value["error"]["message"]
                == "offline recovery verification failed or cleanup is unconfirmed: retain incomplete evidence and the stopped target; do not release ingress",
        "incorrect CLI error boundary"
    );
    ensure!(
        !report.exists()
            && !String::from_utf8_lossy(&out).contains("AMBIENT_SECRET_SENTINEL")
            && !String::from_utf8_lossy(&err).contains("AMBIENT_SECRET_SENTINEL"),
        "ambient config read or premature report publication"
    );
    let mut files = std::fs::read_dir(directory.path())?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    files.sort();
    ensure!(
        files == ["bad-plan.json", "bitrouter.yaml"].map(std::ffi::OsString::from),
        "offline command discovered/created extra state"
    );
    Ok(())
}
