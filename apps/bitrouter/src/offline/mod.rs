//! Explicit local evidence and migration adapters for `bro tenancy offline`.
//!
//! Evidence is application-private, not an AI/SDK DTO, runtime authority grant
//! or authorization to release ingress or complete cutover.
pub(crate) mod audit;
pub(crate) mod engine;
#[cfg(feature = "core")]
pub(crate) mod evaluation;
mod files;
mod format;
mod inventory;
mod mapping;
pub(crate) mod recovery;
mod schema;

use crate::{output::CliReport, tenancy::ownership};
use anyhow::{Result, ensure};
use format::{EXCLUSIONS, Evidence, FORMAT, MAX_EVIDENCE, MAX_FILE, Report};
use futures::FutureExt;
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

/// Create secured, incomplete-for-cutover evidence from one explicitly selected DB.
///
/// # Errors
/// Rejects non-Unix, missing/busy/hard-linked sources, existing output, unsupported
/// SQLite contents and every exceeded bound. Failures before publication retain
/// an incomplete bundle; output failure after publication does not undo evidence.
pub async fn snapshot(database: &Path, bundle: &Path) -> Result<impl CliReport> {
    snapshot_inner(database,bundle).await.map_err(|_| failure(
        "offline snapshot failed: require a stopped, exclusively owned regular SQLite source and a new private bundle path; unsupported/corrupt/oversized data is refused; any INCOMPLETE bundle is unusable",
    ))
}
/// Recompute and display the exact DB inventory without asserting evidence origin.
///
/// # Errors
/// Refuses incomplete, altered, insecure, unsupported or oversized bundles.
pub async fn inventory(bundle: &Path) -> Result<impl CliReport> {
    read_bundle(bundle,None,"inventory").await.map_err(|_| failure(
        "offline inventory failed: bundle is incomplete, insecure, corrupt, changed or outside the supported DB-only profile",
    ))
}
/// Verify a bundle against the evidence digest retained outside that bundle.
///
/// # Errors
/// Refuses digest mismatch and every failure of inventory/profile/file validation.
pub async fn verify(bundle: &Path, evidence_digest: &str) -> Result<impl CliReport> {
    files::validate_digest(evidence_digest).map_err(|_| {
        failure("evidence-digest requires exactly 64 lowercase hexadecimal characters")
    })?;
    read_bundle(bundle,Some(evidence_digest),"verify").await.map_err(|_| failure(
        "offline verification failed: evidence anchor, private profile, complete file set or actual database inventory does not match; no recovery is authorized",
    ))
}
pub(super) fn failure(message: &'static str) -> anyhow::Error {
    // Do not forward SQL/JSON/OS errors: their details can contain source payload
    // or schema defaults. The CLI emits exactly one standard nonsecret error.
    crate::gateway::error::BitrouterError::bad_request(message).into()
}

enum Action {
    Snapshot {
        database: PathBuf,
        bundle: PathBuf,
    },
    Read {
        bundle: PathBuf,
        expected: Option<String>,
        action: &'static str,
    },
}

// A cancelled public future abandons only its reply. The task keeps canonical
// ownership and every connection until SQLx's actual worker close is confirmed.
async fn run(action: Action) -> Result<Report> {
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let (reply, receive) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let owner = match &action {
            Action::Snapshot { database, .. } => match ownership::acquire_path(database, false) {
                Ok(owner) => Some(owner),
                Err(error) => {
                    if reply.send(Err(error)).is_err() {
                        tracing::debug!("offline reply abandoned");
                    }
                    return;
                }
            },
            Action::Read { .. } => None,
        };
        let mut connections = engine::Connections::new(cancel, owner);
        let result = AssertUnwindSafe(async {
            connections.check()?;
            match action {
                Action::Snapshot { bundle, .. } => snapshot_work(&bundle, &mut connections).await,
                Action::Read {
                    bundle,
                    expected,
                    action,
                } => read_bundle_work(&bundle, expected.as_deref(), action, &mut connections).await,
            }
        })
        .catch_unwind()
        .await;
        let result = match result {
            Ok(result) => result,
            Err(_) => {
                connections.uncertain = true;
                Err(anyhow::anyhow!("offline work unwound"))
            }
        };
        let cleanup = AssertUnwindSafe(connections.close()).catch_unwind().await;
        let confirmed = matches!(cleanup, Ok(Ok(()))) && !connections.uncertain;
        if !confirmed {
            if reply
                .send(Err(anyhow::anyhow!("offline driver cleanup unconfirmed")))
                .is_err()
            {
                tracing::debug!("offline cleanup reply abandoned");
            }
            drop(connections); // Unconfirmed cleanup retains its source fence.
            return;
        }
        drop(connections);
        if reply.send(result).is_err() {
            tracing::debug!("offline reply abandoned after cleanup");
        }
    });
    // Dropping this handle never aborts the task. The task is the sole connection
    // owner; no public handle can cancel its SQL await or cleanup.
    let response = receive
        .await
        .map_err(|_| anyhow::anyhow!("offline worker unavailable"))?;
    task.await
        .map_err(|_| anyhow::anyhow!("offline worker ended unexpectedly"))?;
    response
}

async fn snapshot_inner(database: &Path, bundle: &Path) -> Result<Report> {
    ensure!(cfg!(unix), "Unix only");
    run(Action::Snapshot {
        database: database.into(),
        bundle: bundle.into(),
    })
    .await
}

async fn read_bundle(
    bundle: &Path,
    expected: Option<&str>,
    action: &'static str,
) -> Result<Report> {
    ensure!(cfg!(unix), "Unix only");
    run(Action::Read {
        bundle: bundle.into(),
        expected: expected.map(str::to_owned),
        action,
    })
    .await
}

async fn snapshot_work(bundle: &Path, connections: &mut engine::Connections) -> Result<Report> {
    connections.check()?;
    let source_path = connections
        .owner
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("source owner missing"))?
        .path
        .clone();
    files::create_directory(bundle)?;
    let bundle = bundle.canonicalize()?;
    let mut key = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| anyhow::anyhow!("offline entropy unavailable"))?;
    files::write_new(&bundle.join("inventory.key"), &key)?;
    let copy = bundle.join("snapshot.sqlite");
    // VACUUM INTO accepts an empty existing destination. Pre-create it with
    //0600 rather than depend on process umask during SQLite's output creation.
    // https://www.sqlite.org/lang_vacuum.html#vacuum_with_an_into_clause
    files::write_new(&copy, &[])?;
    connections.open(&source_path, false).await?;
    let result = async {
        connections.check()?;
        let source = connections
            .source
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("source unavailable"))?;
        let before = inventory::inspect(source, &key).await?;
        let destination = copy
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("bundle path is not UTF-8"))?;
        // All source schema was qualified before any integrity/VACUUM work.
        connections.check()?;
        let source = connections
            .source
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("source unavailable"))?;
        sea_orm::sqlx::query("VACUUM INTO ?")
            .bind(destination)
            .execute(&mut *source)
            .await?;
        let after = inventory::inspect(source, &key).await?;
        ensure!(before == after, "source changed during snapshot");
        connections.open(&copy, true).await?;
        let copied = connections
            .copy
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("copy unavailable"))?;
        let copied_inventory = inventory::inspect(copied, &key).await;
        ensure!(
            before == copied_inventory?,
            "snapshot inventory differs from source"
        );
        Ok::<_, anyhow::Error>(before)
    }
    .await;
    connections.close().await?;
    let inventory = result?;
    connections.check()?;
    let (snapshot_bytes, snapshot_digest) = files::snapshot_digest(&copy, &key)?;
    files::private_file(&copy, MAX_FILE)?.sync_all()?;
    let mut selected_path = files::Fingerprint::new(&key, b"selected-source-path")?;
    selected_path.field(source_path.as_os_str().as_encoded_bytes());
    let evidence = Evidence {
        format: FORMAT.into(),
        scope: "sqlite_database_only".into(),
        complete: false,
        cutover_ready: false,
        exclusions: EXCLUSIONS.iter().map(|v| (*v).into()).collect(),
        inspector: crate::artifact::current(),
        source_commit: None,
        source_build: None,
        selected_path_digest: hex::encode(selected_path.finish()),
        key_id: files::sha(&key),
        snapshot_bytes,
        snapshot_digest,
        inventory,
    };
    evidence.validate_profile()?;
    let bytes = files::evidence_bytes(&evidence)?;
    let evidence_digest = files::sha(&bytes);
    connections.check()?;
    files::write_new(&bundle.join("evidence.json"), &bytes)?;
    files::sync_directory(&bundle)?;
    files::write_new(
        &bundle.join("READY"),
        format!("{evidence_digest}\n").as_bytes(),
    )?;
    files::sync_directory(&bundle)?;
    connections.check()?;
    std::fs::remove_file(bundle.join("INCOMPLETE"))?;
    files::sync_directory(&bundle)?;
    if let Some(parent) = bundle.parent() {
        files::sync_directory(parent)?;
    }
    Ok(Report {
        action: "snapshot",
        evidence_digest,
        evidence_anchor_checked: false,
        database_inventory_verified: true,
        evidence,
    })
}
async fn read_bundle_work(
    bundle: &Path,
    expected: Option<&str>,
    action: &'static str,
    connections: &mut engine::Connections,
) -> Result<Report> {
    ensure!(cfg!(unix), "Unix only");
    files::private_directory(bundle)?;
    let expected_files = ["READY", "evidence.json", "inventory.key", "snapshot.sqlite"];
    let mut names = Vec::new();
    for entry in std::fs::read_dir(bundle)? {
        let entry = entry?;
        ensure!(
            names.len() < expected_files.len(),
            "unexpected bundle files"
        );
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("unsupported bundle entry"))?;
        ensure!(
            expected_files.contains(&name.as_str()),
            "incomplete or unexpected bundle files"
        );
        names.push(name);
    }
    names.sort();
    ensure!(names == expected_files, "incomplete bundle");
    let ready = files::private_file(&bundle.join("READY"), 65)?;
    ready.try_lock_shared()?;
    let key: [u8; 32] = files::read(&bundle.join("inventory.key"), 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid inventory key"))?;
    let bytes = files::read(&bundle.join("evidence.json"), MAX_EVIDENCE)?;
    let evidence_digest = files::sha(&bytes);
    ensure!(
        expected.is_none_or(|v| v == evidence_digest),
        "evidence anchor mismatch"
    );
    ensure!(
        files::read(&bundle.join("READY"), 65)? == format!("{evidence_digest}\n").as_bytes(),
        "ready marker mismatch"
    );
    let evidence: Evidence = serde_json::from_slice(&bytes)?;
    evidence.validate_profile()?;
    ensure!(
        evidence.key_id == files::sha(&key),
        "inventory key mismatch"
    );
    let copy = bundle.join("snapshot.sqlite");
    let expected_file = (evidence.snapshot_bytes, evidence.snapshot_digest.clone());
    ensure!(
        files::snapshot_digest(&copy, &key)? == expected_file,
        "snapshot file mismatch"
    );
    connections.open(&copy, true).await?;
    let db = connections
        .copy
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("copy unavailable"))?;
    #[cfg(feature = "core")]
    let actual = if action.starts_with("eval-") {
        inventory::inspect_eval(db, &key).await
    } else {
        inventory::inspect(db, &key).await
    };
    #[cfg(not(feature = "core"))]
    let actual = inventory::inspect(db, &key).await;
    connections.close().await?;
    connections.check()?;
    ensure!(actual? == evidence.inventory, "inventory mismatch");
    ensure!(
        files::snapshot_digest(&copy, &key)? == expected_file,
        "snapshot changed during verification"
    );
    ensure!(
        files::sha(&files::read(&bundle.join("evidence.json"), MAX_EVIDENCE)?) == evidence_digest,
        "evidence changed during verification"
    );
    Ok(Report {
        action,
        evidence_digest,
        evidence_anchor_checked: expected.is_some(),
        database_inventory_verified: true,
        evidence,
    })
}

#[cfg(all(test, unix))]
mod tests;

/// Serve only archived receipt reads from a verified DB-only bundle.
///
/// # Errors
/// Refuses unsupported/insecure archives, reused projections, invalid control
/// configuration, non-loopback listeners and unconfirmed connection cleanup.
pub async fn audit(
    archive: &Path,
    projection: &Path,
    evidence_digest: &str,
    control_config: &Path,
    listen: std::net::SocketAddr,
    output: &crate::output::Output,
) -> Result<()> {
    audit::run(
        archive,
        projection,
        evidence_digest,
        control_config,
        listen,
        output,
    )
    .await
}

/// Validate a frozen M0 mapping manifest against a DB-only evidence bundle.
///
/// This records operator claims without backfill or ownership authentication.
/// Per-owner counts, external evidence, producer identity and cutover remain
/// unverified. The report is not an authority capability.
///
/// # Errors
/// Rejects unsupported platforms, bounds, syntax, duplicate/conflicting keys,
/// unmatched approvals, snapshot/installation mismatch and insecure output.
pub async fn map(
    bundle: &Path,
    evidence_digest: &str,
    manifest: &Path,
    report: &Path,
) -> Result<impl CliReport> {
    mapping::run(bundle, evidence_digest, manifest, report).await.map_err(|_| failure(
        "offline mapping validation failed: require a private bounded M0 manifest, matching evidence snapshot and a new report directory; no source rows were backfilled or authority granted",
    ))
}

/// Partition a verified Legacy Eval graph into explicit owner namespaces.
///
/// # Errors
/// Refuses mixed ownership, corrupt graphs, bounds or insecure evidence/output.
#[cfg(feature = "core")]
pub async fn eval_inventory(
    bundle: &Path,
    evidence_digest: &str,
    source_kind: &str,
    report: &Path,
) -> Result<impl CliReport> {
    evaluation::inventory(bundle,evidence_digest,source_kind,report).await.map_err(|_|failure(
        "offline Eval inventory failed: require exact Legacy ownership, a complete bounded graph and matching private evidence"))
}

/// Import a bounded four-table Legacy Eval graph into a stopped fresh authority.
///
/// # Errors
/// Refuses any mapping/evidence/target mismatch, collision, unsupported state or
/// unconfirmed SQL outcome. Failure never proves no-write; retain INCOMPLETE and
/// reconcile before restarting. This command does not authorize cutover.
#[cfg(feature = "core")]
pub async fn eval_backfill(
    plan: &Path,
    plan_digest: &str,
    report: &Path,
) -> Result<impl CliReport> {
    evaluation::backfill(plan,plan_digest,report).await.map_err(|_|failure(
        "offline Eval backfill failed or outcome is unconfirmed: retain the incomplete packet and stopped destination for reconciliation; do not retry or activate"))
}

/// Verify an explicitly retired archive and freshly provisioned stopped target.
///
/// # Errors
/// Rejects incomplete evidence, changed inputs, unsupported profiles or
/// unconfirmed worker cleanup. Never initializes authority or authorizes cutover.
pub async fn recover(
    plan: &std::path::Path,
    report: &std::path::Path,
) -> anyhow::Result<impl crate::output::CliReport> {
    recovery::run(plan, report).await.map_err(|_| failure("offline recovery verification failed or cleanup is unconfirmed: retain incomplete evidence and the stopped target; do not release ingress"))
}
