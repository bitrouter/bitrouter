//! Verify a declared fresh installation without creating a runtime owner or writer.
mod catalog;
mod inputs;
pub(crate) mod plan;
#[cfg(all(test, unix))]
mod tests;

use super::{engine::Connections, files, format, inventory};
use anyhow::{Context, Result, ensure};
use futures::FutureExt;
use serde::Serialize;
use std::{
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[derive(Serialize)]
pub(crate) struct AuthorityFacts {
    pub installation_id: bitrouter_ai::tenancy::identity::InstallationId,
    pub authority_revision: u64,
    pub ordinary_operations: u32,
    pub safety_operations: u32,
    pub saturated: bool,
}
#[derive(Serialize)]
pub(super) struct Report {
    format: &'static str,
    action: &'static str,
    plan_digest: String,
    database_evidence_digest: String,
    source: AuthorityFacts,
    source_tables: Vec<plan::Disposition>,
    target: AuthorityFacts,
    target_rows: u64,
    target_value_bytes: u64,
    target_schema_digest: String,
    target_inventory_sha256: String,
    target_file_digest: String,
    selected_files: Vec<inputs::FileRecord>,
    source_producer_attestation_bound: bool,
    operational_fence_attestation_bound: bool,
    external_writers_independently_excluded: bool,
    fresh_provisioning_verified: bool,
    static_closure_authenticated: bool,
    history_transferred: bool,
    upstream_effects_resolved: bool,
    complete: bool,
    cutover_ready: bool,
}
impl crate::output::CliReport for Report {
    fn render(&self, output: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        output.line("Declared fresh provisioning and static closure verified read-only. No history transferred or ingress released.")?;
        output.line("Retirement remains an operator-maintained fence; this report is not runtime authority or cutover readiness.")?;
        output.line(&format!("Private plan SHA256: {}", self.plan_digest))
    }
}

struct Work {
    inputs: inputs::Inputs,
    key: [u8; 32],
    source: AuthorityFacts,
    target: AuthorityFacts,
    inventory: format::Inventory,
    target_path: PathBuf,
    target_digest: (u64, String),
    output: PathBuf,
}

pub(super) async fn run(path: &Path, output: &Path) -> Result<Report> {
    run_inner(
        path,
        output,
        #[cfg(all(test, unix))]
        None,
    )
    .await
}
async fn run_inner(
    path: &Path,
    output: &Path,
    #[cfg(all(test, unix))] held: Option<tests::Hold>,
) -> Result<Report> {
    ensure!(cfg!(unix), "recovery profile is Unix local SQLite only");
    plan::absolute(path)?;
    plan::absolute(output)?;
    let started = Instant::now();
    let bytes = files::read(path, format::MAX_EVIDENCE)?;
    let plan: plan::Plan = serde_json::from_slice(&bytes)?;
    plan.validate()?;
    let path = path.to_path_buf();
    let output = output.to_path_buf();
    let cancel = CancellationToken::new();
    let _abandoned = cancel.clone().drop_guard();
    let (send, wait) = tokio::sync::oneshot::channel();
    // A dropped CLI/API waiter requests cancellation; it never cancels the
    // actual SQLx establishment/read/rollback/close future or releases its lease.
    let task = tokio::spawn(async move {
        let owner = match crate::tenancy::ownership::acquire_path(&plan.target.database, false) {
            Ok(owner) => owner,
            Err(error) => {
                let _ = send.send(Err(error));
                return;
            }
        };
        let mut connections = Connections::new(cancel.clone(), Some(owner));
        let check = || {
            ensure!(
                !cancel.is_cancelled() && started.elapsed() < Duration::from_secs(120),
                "recovery operation ended"
            );
            Ok(())
        };
        let outcome = AssertUnwindSafe(work(
            &mut connections,
            &path,
            &bytes,
            &output,
            &check,
            #[cfg(all(test, unix))]
            held,
        ))
        .catch_unwind()
        .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(_) => {
                connections.uncertain = true;
                Err(anyhow::anyhow!("recovery inspection unwound"))
            }
        };
        let closed = AssertUnwindSafe(connections.close()).catch_unwind().await;
        let result = if !matches!(closed, Ok(Ok(()))) || connections.uncertain {
            Err(anyhow::anyhow!(
                "recovery worker cleanup unconfirmed; retain target owner"
            ))
        } else {
            outcome.and_then(|work| publish(work, &check))
        };
        // Publication/rechecks finish under the original canonical lease, after
        // the real worker ACK. Unconfirmed close keeps that lease until exit.
        drop(connections);
        if send.send(result).is_err() {
            tracing::debug!("recovery reply abandoned after owned cleanup");
        }
    });
    let result = wait.await.context("recovery owner unavailable")?;
    task.await.context("recovery owner join unavailable")?;
    result
}

fn private_target(path: &Path) -> Result<()> {
    // No separate DB descriptor is opened while SQLite owns one.
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= format::MAX_FILE,
        "target file profile"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            metadata.nlink() == 1 && metadata.permissions().mode() & 0o077 == 0,
            "target file must already be private"
        );
    }
    files::private_directory(path.parent().context("target parent")?)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut file = path.as_os_str().to_os_string();
        file.push(suffix);
        match std::fs::symlink_metadata(PathBuf::from(file)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => anyhow::bail!("target is not a stopped closed SQLite file"),
        }
    }
    Ok(())
}

async fn work(
    c: &mut Connections,
    path: &Path,
    original: &[u8],
    output: &Path,
    check: &(dyn Fn() -> Result<()> + Sync),
    #[cfg(all(test, unix))] held: Option<tests::Hold>,
) -> Result<Work> {
    check()?;
    let plan: plan::Plan = serde_json::from_slice(original)?;
    let target = c
        .owner
        .as_ref()
        .context("recovery target lease")?
        .path
        .clone();
    private_target(&target)?;
    let source = plan.source.bundle.canonicalize()?;
    ensure!(
        !target.starts_with(&source),
        "target inside immutable archive"
    );
    let parent = output.parent().context("report parent")?.canonicalize()?;
    files::private_directory(&parent)?;
    let output = parent.join(output.file_name().context("report name")?);
    ensure!(
        !output.starts_with(&source) && !target.starts_with(&output),
        "report overlaps input store"
    );
    files::create_directory(&output)?;
    let key: [u8; 32] = files::read(&source.join("inventory.key"), 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("inventory key width"))?;
    let inputs = inputs::Inputs::read(path, &target, &key)?;
    ensure!(
        inputs.plan_bytes == original,
        "plan changed after owner acquisition"
    );
    inputs.write(&output)?;
    check()?;
    let target_digest = files::snapshot_digest(&target, &key)?;
    let verified =
        super::read_bundle_work(&source, Some(&plan.source.evidence_digest), "recover", c).await?;
    catalog::dispositions(&plan.source, &verified.evidence.inventory)?;
    check()?;
    // Target is explicitly closed (no WAL/SHM/journal), under its canonical
    // exclusive lease. Immutable reads cannot create SQLite sidecars.
    c.open(&target, true).await?;
    c.uncertain = true;
    sea_orm::sqlx::query("BEGIN")
        .execute(c.copy.as_mut().context("target connection")?)
        .await?;
    let target_result = async {
        #[cfg(all(test, unix))]
        if let Some(held) = held {
            held.entered.cancel();
            held.release.cancelled().await;
        }
        check()?;
        let db = c.copy.as_mut().context("target connection")?;
        catalog::preflight(db).await?;
        let inventory = inventory::inspect_in(db, &key).await?;
        ensure!(
            inventory.schema_digest == verified.evidence.inventory.schema_digest,
            "fresh target schema differs from the explicitly evidenced producer schema"
        );
        catalog::fresh(db, &inventory, &inputs.plan.target).await?;
        let hashes = catalog::credential_hashes(db).await?;
        let checked = || {
            check().map_err(|_| bitrouter_ai::tenancy::operation::ErrorCode::AuthorityUnavailable)
        };
        let budget = crate::storage::operation_budget::OperationBudget::new(&checked);
        budget.charge(
            usize::try_from(inventory.rows)?,
            usize::try_from(inventory.value_bytes)?,
        )?;
        let facts = crate::tenancy::audit::recovery::inspect_target(
            db,
            &inputs.plan.target,
            &inputs.provisioning,
            &inputs.manifest,
            &budget,
        )
        .await?;
        check()?;
        Ok::<_, anyhow::Error>((inventory, hashes, facts))
    }
    .await;
    c.cleanup_budget().await?;
    sea_orm::sqlx::query("ROLLBACK")
        .execute(c.copy.as_mut().context("target rollback connection")?)
        .await?;
    c.uncertain = false;
    c.close().await?;
    let (inventory, hashes, target_facts) = target_result?;
    check()?;
    c.open(&source.join("snapshot.sqlite"), true).await?;
    let source_result = async {
        let db = c.copy.as_mut().context("archive connection")?;
        catalog::no_copied_credentials(db, &hashes).await?;
        crate::tenancy::audit::recovery::inspect_source(
            db,
            &inputs.plan.source.installation_id,
            check,
        )
        .await
    }
    .await;
    c.close().await?;
    let source_facts = source_result?;
    check()?;
    // Recompute the frozen source evidence with the same original command
    // budget. No new owning task/deadline or source writer is introduced.
    let after =
        super::read_bundle_work(&source, Some(&plan.source.evidence_digest), "recover", c).await?;
    ensure!(
        after.evidence.inventory == verified.evidence.inventory
            && after.evidence.snapshot_digest == verified.evidence.snapshot_digest,
        "source archive changed during recovery verification"
    );
    Ok(Work {
        inputs,
        key,
        source: source_facts,
        target: target_facts,
        inventory,
        target_path: target,
        target_digest,
        output,
    })
}

fn publish(work: Work, check: &dyn Fn() -> Result<()>) -> Result<Report> {
    check()?;
    private_target(&work.target_path)?;
    ensure!(
        files::snapshot_digest(&work.target_path, &work.key)? == work.target_digest,
        "target changed during read-only verification"
    );
    work.inputs.recheck(check)?;
    let inventory_bytes = bounded_json(&work.inventory)?;
    files::write_new(&work.output.join("target-inventory.json"), &inventory_bytes)?;
    let report = Report {
        format: "bitrouter.offline.recovery-evidence.v1",
        action: "recover",
        plan_digest: files::sha(&work.inputs.plan_bytes),
        database_evidence_digest: work.inputs.plan.source.evidence_digest,
        source: work.source,
        source_tables: work.inputs.plan.source.tables,
        target: work.target,
        target_rows: work.inventory.rows,
        target_value_bytes: work.inventory.value_bytes,
        target_schema_digest: work.inventory.schema_digest,
        target_inventory_sha256: files::sha(&inventory_bytes),
        target_file_digest: work.target_digest.1,
        selected_files: work.inputs.files.into_iter().map(|f| f.record).collect(),
        source_producer_attestation_bound: true,
        operational_fence_attestation_bound: true,
        external_writers_independently_excluded: false,
        fresh_provisioning_verified: true,
        static_closure_authenticated: true,
        history_transferred: false,
        upstream_effects_resolved: false,
        complete: false,
        cutover_ready: false,
    };
    let bytes = bounded_json(&report)?;
    check()?;
    files::write_new(&work.output.join("report.json"), &bytes)?;
    files::sync_directory(&work.output)?;
    check()?;
    // This marker authenticates evidence bytes only. It is never a runtime
    // startup/dispatch capability, migration completion, or writer fence.
    files::write_new(
        &work.output.join("VERIFIED"),
        format!("{}\n", files::sha(&bytes)).as_bytes(),
    )?;
    files::sync_directory(&work.output)?;
    std::fs::remove_file(work.output.join("INCOMPLETE"))?;
    files::sync_directory(&work.output)?;
    files::sync_directory(work.output.parent().context("report parent")?)?;
    Ok(report)
}

fn bounded_json(value: &impl Serialize) -> Result<Vec<u8>> {
    struct Output(Vec<u8>);
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > (format::MAX_EVIDENCE as usize).saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("recovery evidence bound"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Output(Vec::new());
    serde_json::to_writer_pretty(&mut output, value)?;
    Ok(output.0)
}
