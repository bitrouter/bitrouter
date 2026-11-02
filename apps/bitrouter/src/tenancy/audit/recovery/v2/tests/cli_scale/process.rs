//! The qualification retains every CLI/drain handle until actual exit/join.

use super::*;
use crate::offline::recovery::tests::fixture::scale::observation::Observation;
use std::process::{ExitStatus, Stdio};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::oneshot,
    task::JoinHandle,
    time::{Instant, timeout_at},
};

const OUTPUT: usize = 64 * 1024;
// Full migration column metadata is larger than a readiness/error response.
// This fixed fixture drain allowance is independent of the million data rows.
const INVENTORY_OUTPUT: usize = 2 * 1024 * 1024;
pub(super) const TOKEN: &str = "offline-scale-audit-control-0123456789abcdef";

pub(super) fn command(binary: &Path, home: &Path, action: &str) -> Result<Command> {
    let mut command = Command::new(binary);
    command
        .args(["--json", "tenancy", "offline", action])
        .args([
            "--working-bytes",
            "67108864",
            "--scratch-free-bytes",
            "2147483648",
        ])
        .arg("--scratch-directory")
        .arg(home)
        .env_clear()
        .env("BITROUTER_HOME", home)
        .env("RUST_LOG", "error")
        .env("OFFLINE_SCALE_AUDIT_TOKEN", TOKEN)
        .env(producer::KEY_ENV, std::env::var(producer::KEY_ENV)?)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

async fn drain(input: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input
        .take(u64::try_from(limit + 1)?)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= limit,
        "CLI output exceeded fixture drain allowance"
    );
    Ok(bytes)
}

async fn audit_stdout(
    mut input: tokio::process::ChildStdout,
    ready: oneshot::Sender<Value>,
) -> Result<Vec<u8>> {
    let mut ready = Some(ready);
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        ensure!(
            bytes.len().checked_add(count).is_some_and(|n| n <= OUTPUT),
            "audit stdout exceeded allowance"
        );
        bytes.extend_from_slice(&buffer[..count]);
        if ready.is_some() {
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) => {
                    if let Some(sender) = ready.take() {
                        sender
                            .send(value)
                            .map_err(|_| anyhow::anyhow!("audit readiness observer abandoned"))?;
                    }
                }
                Err(error) if error.is_eof() => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    ensure!(ready.is_none(), "audit exited before readiness");
    Ok(bytes)
}

fn redacted(bytes: &[u8], secrets: &[&str]) -> Result<()> {
    for secret in secrets
        .iter()
        .copied()
        .chain([TOKEN, "PRIVATE_RECOVERY_PROVIDER_KEY"])
    {
        ensure!(!secret.is_empty());
        ensure!(
            !bytes
                .windows(secret.len())
                .any(|part| part == secret.as_bytes()),
            "CLI exposed fixture secret"
        );
    }
    Ok(())
}

async fn reap(child: &mut Child, graceful: bool, deadline: Instant) -> Result<ExitStatus> {
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }
    let signal = if graceful {
        let pid = rustix::process::Pid::from_raw(i32::try_from(child.id().context("audit PID")?)?)
            .context("invalid audit PID")?;
        rustix::process::kill_process(pid, rustix::process::Signal::TERM)
            .map_err(anyhow::Error::from)
    } else {
        child.start_kill().map_err(anyhow::Error::from)
    };
    // A failed signal still goes through actual wait/force-kill before propagation.
    let normal = timeout_at(
        deadline.min(Instant::now() + Duration::from_secs(30)),
        child.wait(),
    )
    .await;
    match normal {
        Ok(result) => {
            let status = result?;
            signal?;
            Ok(status)
        }
        Err(_) => {
            let killed = child.start_kill();
            let reaped = timeout_at(deadline, child.wait())
                .await
                .context("CLI child cleanup unconfirmed")?;
            reaped?;
            killed?;
            anyhow::bail!("CLI did not acknowledge requested shutdown before force-kill")
        }
    }
}

async fn joined(
    child: &mut Child,
    out: &mut JoinHandle<Result<Vec<u8>>>,
    err: &mut JoinHandle<Result<Vec<u8>>>,
    graceful: bool,
    deadline: Instant,
    secrets: &[&str],
) -> Result<(ExitStatus, Vec<u8>)> {
    // Never short-circuit past another retained handle on an earlier error.
    let status = reap(child, graceful, deadline).await;
    let stdout = timeout_at(deadline, out).await;
    let stderr = timeout_at(deadline, err).await;
    let status = status?;
    let stdout = stdout.context("CLI stdout join unconfirmed")???;
    let stderr = stderr.context("CLI stderr join unconfirmed")???;
    redacted(&stdout, secrets)?;
    redacted(&stderr, secrets)?;
    Ok((status, stdout))
}

pub(super) async fn invoke(
    mut command: Command,
    label: &str,
    work_until: Instant,
    cleanup_until: Instant,
    cancel: &CancellationToken,
    secrets: &[&str],
) -> Result<Value> {
    ensure!(
        !cancel.is_cancelled() && Instant::now() < work_until,
        "CLI aggregate work deadline"
    );
    let observation = Observation::new(command.as_std().get_current_dir().context("CLI home")?)?;
    let mut child = command.spawn()?;
    observation.attach(child.id());
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        child.kill().await?;
        anyhow::bail!("CLI pipe setup missing");
    };
    let mut out = tokio::spawn(drain(stdout, INVENTORY_OUTPUT));
    let mut err = tokio::spawn(drain(stderr, OUTPUT));
    let mut cleanup_confirmed = false;
    let result = {
        let work = async {
            let outcome = tokio::select! {
                _ = cancel.cancelled() => Err(anyhow::anyhow!("CLI qualification cancelled")),
                result = timeout_at(work_until.min(Instant::now()+Duration::from_secs(300)),child.wait()) =>
                    result.context("CLI phase deadline").and_then(|status| status.map_err(anyhow::Error::from)),
            };
            let cleanup = joined(
                &mut child,
                &mut out,
                &mut err,
                false,
                cleanup_until,
                secrets,
            )
            .await;
            cleanup_confirmed = cleanup.is_ok();
            let (status, bytes) = cleanup?;
            let waited = outcome?;
            ensure!(
                waited == status && status.success(),
                "offline CLI exited unsuccessfully"
            );
            Ok::<Value, anyhow::Error>(serde_json::from_slice(&bytes)?)
        };
        tokio::pin!(work);
        loop {
            tokio::select! {
                result = &mut work => break result,
                _ = observation.tick() => {}
            }
        }
    };
    let measured = observation.finish(label, cleanup_confirmed, false);
    let value = result?;
    measured?;
    Ok(value)
}

pub(super) async fn audit(
    mut command: Command,
    label: &str,
    installation: &InstallationId,
    work_until: Instant,
    cleanup_until: Instant,
    cancel: &CancellationToken,
    secrets: &[&str],
) -> Result<()> {
    ensure!(
        !cancel.is_cancelled() && Instant::now() < work_until,
        "audit aggregate work deadline"
    );
    let observation = Observation::new(command.as_std().get_current_dir().context("CLI home")?)?;
    let mut child = command.spawn()?;
    observation.attach(child.id());
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        child.kill().await?;
        anyhow::bail!("audit pipe setup missing");
    };
    let (send, wait) = oneshot::channel();
    let mut out = tokio::spawn(audit_stdout(stdout, send));
    let mut err = tokio::spawn(drain(stderr, OUTPUT));
    let mut cleanup_confirmed = false;
    let result = {
        let observed = async {
            let work = async {
                let ready = wait.await.context("audit readiness failed")?;
                ensure!(
                    ready["action"] == "audit" && ready["installation_id"] == installation.as_str()
                );
                ensure!(ready["complete"] == false && ready["cutover_ready"] == false);
                let address: std::net::SocketAddr =
                    ready["listen"].as_str().context("audit address")?.parse()?;
                ensure!(address.ip().is_loopback());
                let endpoint = format!("http://{address}");
                let client = bitrouter_sdk::tenancy::TenancyClient::new(&endpoint, TOKEN)?;
                let page = client
                    .executions(
                        installation,
                        &"tenant".try_into()?,
                        &"scale-audit".try_into()?,
                        None,
                        10,
                    )
                    .await?;
                ensure!(page.installation_id == *installation && page.execution_ids.is_empty());
                let response = reqwest::Client::builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(10))
                    .build()?
                    .post(format!("{endpoint}/control/v1/tenants/tenant"))
                    .bearer_auth(TOKEN)
                    .header("bitrouter-installation-id", installation.as_str())
                    .send()
                    .await?;
                ensure!(
                    response.status() == reqwest::StatusCode::NOT_FOUND,
                    "archive exposed mutation route"
                );
                Ok::<_, anyhow::Error>(())
            };
            let outcome = tokio::select! {
                _ = cancel.cancelled() => Err(anyhow::anyhow!("audit qualification cancelled")),
                result = timeout_at(work_until.min(Instant::now()+Duration::from_secs(300)),work) =>
                    result.context("audit readiness/read deadline").and_then(|result| result),
            };
            let cleanup =
                joined(&mut child, &mut out, &mut err, true, cleanup_until, secrets).await;
            cleanup_confirmed = cleanup.is_ok();
            let (status, _) = cleanup?;
            ensure!(status.success(), "audit did not exit gracefully");
            outcome
        };
        tokio::pin!(observed);
        loop {
            tokio::select! {
                result = &mut observed => break result,
                _ = observation.tick() => {}
            }
        }
    };
    let measured = observation.finish(label, cleanup_confirmed, false);
    result?;
    measured
}
