//! Actual archive CLI consumer; its mutable projection is outside the frozen bundle.
use super::{
    engine::Connections,
    files,
    format::{MAX_FILE, Report},
    read_bundle_work,
};
use crate::{
    config::{ControlConfig, ControlCredentialConfig},
    output::{CliReport, Format, Output},
    remote_control::auth::ControlAuth,
    tenancy::audit::Archive,
};
use anyhow::{Context, Result, ensure};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    net::SocketAddr,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlFile {
    credentials: Vec<ControlCredentialConfig>,
}
#[derive(Serialize)]
struct Ready {
    action: &'static str,
    listen: SocketAddr,
    installation_id: String,
    evidence_digest: String,
    scope: &'static str,
    complete: bool,
    cutover_ready: bool,
}
impl CliReport for Ready {
    fn render(&self, human: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        human.line(&format!(
            "Read-only receipt audit listening on {}",
            self.listen
        ))?;
        human.line(&format!("Retired installation: {}", self.installation_id))?;
        human.line("DB-only archive; not recovery or cutover readiness.")
    }
}
struct Input {
    archive: PathBuf,
    projection: PathBuf,
    digest: String,
    controls: ControlAuth,
    listen: SocketAddr,
    output: Format,
}

pub(super) async fn run(
    archive: &Path,
    projection: &Path,
    digest: &str,
    control: &Path,
    listen: SocketAddr,
    output: &Output,
) -> Result<()> {
    let result = run_inner(archive, projection, digest, control, listen, output).await;
    result.map_err(|_| super::failure("archive audit failed: require verified supported DB evidence, a new private projection outside the bundle, explicit control:tenancy credentials and loopback address; unconfirmed SQLite cleanup retains ownership until process exit"))
}
async fn run_inner(
    archive: &Path,
    projection: &Path,
    digest: &str,
    control: &Path,
    listen: SocketAddr,
    output: &Output,
) -> Result<()> {
    ensure!(cfg!(unix), "archive audit is Unix only");
    ensure!(
        listen.ip().is_loopback(),
        "archive listener must be loopback"
    );
    files::validate_digest(digest)?;
    let config: ControlFile = serde_json::from_slice(&files::read(control, 64 * 1024)?)?;
    ensure!(
        !config.credentials.is_empty() && config.credentials.len() <= 8,
        "explicit bounded control credentials required"
    );
    ensure!(
        config.credentials.iter().all(|entry| entry
            .scopes
            .contains(&bitrouter_ai::control::ControlScope::Tenancy)),
        "archive credentials require control:tenancy"
    );
    let controls = ControlAuth::from_lookup(
        &ControlConfig {
            credentials: config.credentials,
            ..Default::default()
        },
        |name| std::env::var(name).ok().filter(|value| value.len() <= 4096),
    )?;
    let stop = CancellationToken::new();
    let _cancel_on_drop = stop.clone().drop_guard();
    let input = Input {
        archive: archive.into(),
        projection: projection.into(),
        digest: digest.into(),
        controls,
        listen,
        output: output.format(),
    };
    let worker_stop = stop.clone();
    let mut task = tokio::spawn(async move { work(input, worker_stop).await });
    tokio::select! {
        result = &mut task => result?,
        signal = interrupted() => {
            stop.cancel();
            // The same task owns preparation and SQL cleanup. Timing out this
            // observer drops only the JoinHandle, never aborts the owner.
            let result = tokio::time::timeout(std::time::Duration::from_secs(30), &mut task)
                .await.context("archive cleanup observer timed out")??;
            signal?;
            result
        },
    }
}
async fn interrupted() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
async fn work(input: Input, stop: CancellationToken) -> Result<()> {
    let archive = open(&input.archive, &input.projection, &input.digest, stop).await?;
    let result = async {
        let listener = tokio::net::TcpListener::bind(input.listen).await?;
        let ready = Ready {
            action: "audit",
            listen: listener.local_addr()?,
            installation_id: archive.installation().to_string(),
            evidence_digest: input.digest,
            scope: "sqlite_database_only_receipt_audit",
            complete: false,
            cutover_ready: false,
        };
        files::write_new(
            &input.projection.join("audit.json"),
            &serde_json::to_vec_pretty(&ready)?,
        )?;
        files::sync_directory(&input.projection)?;
        std::fs::remove_file(input.projection.join("INCOMPLETE"))?;
        files::sync_directory(&input.projection)?;
        Output::new(input.output).emit(&ready)?;
        archive.serve(listener, input.controls).await
    }
    .await;
    archive.shutdown().await?;
    result
}

/// Establishment is owned before the first SQLite await, even if its caller drops.
pub(crate) async fn open(
    archive: &Path,
    projection: &Path,
    digest: &str,
    stop: CancellationToken,
) -> Result<Archive> {
    let archive = archive.to_owned();
    let projection = projection.to_owned();
    let digest = digest.to_owned();
    let cancelled = stop.child_token();
    let cancel_on_drop = cancelled.clone().drop_guard();
    let (reply, wait) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut connections = Connections::new(cancelled.clone(), None);
        let preparation = AssertUnwindSafe(async {
            let report = project(&archive, &projection, &digest, &mut connections).await?;
            connections.check()?;
            let loaded = crate::tenancy::audit::load_existing(&mut connections).await?;
            connections.check()?;
            Ok::<_, anyhow::Error>((loaded, report))
        })
        .catch_unwind()
        .await;
        let result = match preparation {
            Ok(Ok((loaded, report))) => {
                let reader = Archive::start(connections, loaded, cancelled);
                if report.evidence.inventory.installation_id.as_deref()
                    != Some(reader.installation().as_str())
                {
                    match reader.shutdown().await {
                        Ok(()) => Err(anyhow::anyhow!("archive identity mismatch")),
                        Err(error) => Err(error),
                    }
                } else {
                    Ok(reader)
                }
            }
            other => {
                if other.is_err() {
                    connections.uncertain = true;
                }
                match connections.close().await {
                    Err(error) => Err(error),
                    Ok(()) => Err(anyhow::anyhow!("archive preparation failed")),
                }
            }
        };
        if let Err(Ok(reader)) = reply.send(result)
            && reader.shutdown().await.is_err()
        {
            tracing::error!("abandoned archive cleanup unconfirmed");
        }
    });
    let result = tokio::time::timeout(std::time::Duration::from_secs(120), wait)
        .await
        .context("archive preparation observer timed out")?
        .context("archive bootstrap owner unavailable")?;
    if result.is_ok() {
        drop(cancel_on_drop.disarm());
    }
    result
}

/// Verify before copy; validate the actual projection with the same bounded engine.
pub(super) async fn project(
    archive: &Path,
    projection: &Path,
    digest: &str,
    connections: &mut Connections,
) -> Result<Report> {
    let report = read_bundle_work(archive, Some(digest), "audit", connections).await?;
    ensure!(
        report.evidence.inventory.installation_id.is_some(),
        "archive has no installation"
    );
    use sea_orm_migration::MigratorTrait;
    let expected = crate::db::migration::Migrator::migrations()
        .into_iter()
        .map(|m| m.name().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    let ledger = report
        .evidence
        .inventory
        .ledger
        .as_ref()
        .context("archive ledger missing")?;
    let actual = ledger
        .iter()
        .map(|entry| entry.version.clone())
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        actual.len() == ledger.len() && actual == expected,
        "unsupported archive schema ledger"
    );
    let archive = archive.canonicalize()?;
    let parent = projection
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or(Path::new("."), |p| p)
        .canonicalize()?;
    ensure!(
        !parent.starts_with(&archive),
        "projection must be outside immutable bundle"
    );
    let projection = parent.join(
        projection
            .file_name()
            .context("projection filename missing")?,
    );
    files::create_directory(&projection)?;
    let key: [u8; 32] = files::read(&archive.join("inventory.key"), 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid archive key"))?;
    let source = archive.join("snapshot.sqlite");
    let copy = projection.join("audit.sqlite");
    let copy_path = copy.clone();
    // This future is owned by the preparation task. Caller cancellation cannot
    // abandon a copying thread and then publish/reuse its destination.
    tokio::task::spawn_blocking(move || {
        let mut source = files::private_file(&source, MAX_FILE)?;
        files::write_new(&copy_path, &[])?;
        let mut output = std::fs::OpenOptions::new().write(true).open(&copy_path)?;
        let copied = std::io::copy(
            &mut Read::by_ref(&mut source).take(MAX_FILE + 1),
            &mut output,
        )?;
        ensure!(copied <= MAX_FILE, "archive projection exceeds profile");
        output.flush()?;
        output.sync_all()?;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    ensure!(
        files::snapshot_digest(&copy, &key)?
            == (
                report.evidence.snapshot_bytes,
                report.evidence.snapshot_digest.clone()
            ),
        "projection bytes differ from verified archive"
    );
    // Recheck the source after the copy; never add an ownership sidecar there.
    ensure!(
        files::snapshot_digest(&archive.join("snapshot.sqlite"), &key)?
            == (
                report.evidence.snapshot_bytes,
                report.evidence.snapshot_digest.clone()
            ),
        "archive changed during projection"
    );
    files::sync_directory(&projection)?;
    connections.check()?;
    connections.owner = Some(crate::tenancy::ownership::acquire_path(&copy, false)?);
    connections.open(&copy, true).await?;
    let db = connections
        .copy
        .as_mut()
        .context("projection connection missing")?;
    let actual = super::inventory::inspect(db, &key).await?;
    ensure!(
        actual == report.evidence.inventory,
        "projection inventory mismatch"
    );
    connections.check()?;
    Ok(report)
}
