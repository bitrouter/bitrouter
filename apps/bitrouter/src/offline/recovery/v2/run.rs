//! Retained read-only recovery owner: complete proof, close, recheck, publish.
use super::{
    format::Plan,
    inputs::{self, Prepared},
    paths, retirement,
};
use crate::offline::{
    ValidationBudget,
    engine::Connections,
    files,
    large::{self, Workspace},
    recovery::AuthorityFacts,
    streamed::BundleCustody,
};
use anyhow::{Context, Result, ensure};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::{
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

#[cfg(all(test, unix))]
#[path = "run_tests.rs"]
mod tests;

#[derive(Serialize)]
pub(crate) struct Report {
    format: &'static str,
    action: &'static str,
    plan_digest: String,
    database_evidence_digest: String,
    source: AuthorityFacts,
    target: AuthorityFacts,
    target_rows: u64,
    target_value_bytes: u64,
    target_schema_digest: String,
    target_file_digest: String,
    selected_files: u64,
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
    fn render(&self, human: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        human.line("Complete declared fresh provisioning and static closure verified read-only. No history transferred or ingress released.")?;
        human.line("Retirement remains an operator-maintained fence; this report is not runtime authority or cutover readiness.")?;
        human.line(&format!("Private plan SHA256: {}", self.plan_digest))
    }
}

pub(crate) async fn run(
    path: &Path,
    output: &Path,
    validation: ValidationBudget,
    options: large::Options,
) -> Result<Report> {
    let path = path.to_path_buf();
    let output = output.to_path_buf();
    let cancel = CancellationToken::new();
    let _abandon = cancel.clone().drop_guard();
    let (send, wait) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let result = owned(&path, &output, validation, &options, cancel).await;
        if send.send(result).is_err() {
            tracing::debug!("streamed recovery observer abandoned after owned cleanup");
        }
    });
    let result = wait.await.context("streamed recovery owner unavailable")?;
    task.await
        .context("streamed recovery owner join unavailable")?;
    result
}

async fn owned(
    path: &Path,
    output: &Path,
    validation: ValidationBudget,
    options: &large::Options,
    cancel: CancellationToken,
) -> Result<Report> {
    let (plan, original, control_bytes) =
        inputs::read_control_before_workspace::<Plan>(path, options.working_bytes)?;
    plan.validate()?;
    let paths = paths::qualify(
        path,
        &plan,
        output,
        options,
        options
            .working_bytes
            .checked_sub(control_bytes)
            .context("control allowance exceeded")?,
        &cancel,
    )?;
    let output = paths.output.as_path();
    let workspace = Arc::new(Workspace::create(options, cancel.clone())?);
    let control_charge = workspace.reserve(control_bytes)?;
    let paths_charge = workspace.reserve(paths::WORKING_BYTES)?;
    let owner = crate::tenancy::ownership::acquire_path(&paths.target, false)?;
    let ready = Arc::new(files::private_file(&paths.source.join("READY"), 65)?);
    ready.try_lock_shared()?;
    let mut custody = BundleCustody {
        connections: Connections::new(cancel.clone(), Some(owner)),
        ready: Some(ready),
    };
    let connections = &mut custody.connections;
    connections.validation_budget().await?;
    let outcome = AssertUnwindSafe(work(
        connections,
        WorkInput {
            path,
            plan: &plan,
            original: &original,
            output,
            paths: &paths,
        },
        validation,
        &workspace,
    ))
    .catch_unwind()
    .await;
    let outcome = match outcome {
        Ok(value) => value,
        Err(_) => {
            connections.uncertain = true;
            Err(anyhow::anyhow!("streamed recovery unwound"))
        }
    };
    let closed = AssertUnwindSafe(connections.close()).catch_unwind().await;
    if !matches!(closed, Ok(Ok(()))) {
        connections.uncertain = true;
        // Connections Drop retains the canonical lease on uncertainty. Scratch
        // cleanup never authorizes another SQL owner or successful publication.
        if let Err(error) = finish(workspace) {
            tracing::warn!(%error, "incomplete recovery scratch cleanup failed");
        }
        return Err(anyhow::anyhow!("streamed recovery SQL cleanup unconfirmed"));
    }
    let report = match outcome {
        Ok(report) => report,
        Err(error) => {
            finish(workspace)?;
            return Err(error);
        }
    };
    workspace.check()?;
    drop(original);
    drop(plan);
    drop(control_charge);
    let charge = inputs::metadata_charge(&report, &workspace)?;
    let bytes = serde_json::to_vec_pretty(&report)?;
    workspace.probe(output, u64::try_from(bytes.len())?)?;
    files::write_new(&output.join("report.json"), &bytes)?;
    files::sync_directory(output)?;
    let marker = format!("{}\n", files::sha(&bytes));
    workspace.probe(output, u64::try_from(marker.len())?)?;
    workspace.check()?;
    // Report allocations remain charged through publication even though scratch
    // is already gone. Charge owns the accounting state independently of it.
    finish(workspace)?;
    ensure!(!cancel.is_cancelled(), "recovery ended before publication");
    large::probe(output, options.min_free_bytes, 4096)?;
    files::write_new(&output.join("VERIFIED"), marker.as_bytes())?;
    files::sync_directory(output)?;
    ensure!(!cancel.is_cancelled(), "recovery ended before publication");
    std::fs::remove_file(output.join("INCOMPLETE"))?;
    files::sync_directory(output)?;
    files::sync_directory(output.parent().context("report parent")?)?;
    drop(custody);
    drop(bytes);
    drop(marker);
    drop(charge);
    drop(paths_charge);
    Ok(report)
}

fn finish(workspace: Arc<Workspace>) -> Result<()> {
    Arc::try_unwrap(workspace)
        .map_err(|_| anyhow::anyhow!("recovery file worker still owns scratch"))?
        .finish()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Producer {
    format: String,
    installation_id: bitrouter_ai::tenancy::identity::InstallationId,
    commit: String,
    artifact_sha256: String,
    database_evidence_digest: String,
}

struct WorkInput<'a> {
    path: &'a Path,
    plan: &'a Plan,
    original: &'a [u8],
    output: &'a Path,
    paths: &'a paths::Paths,
}

async fn work(
    c: &mut Connections,
    input: WorkInput<'_>,
    validation: ValidationBudget,
    workspace: &Arc<Workspace>,
) -> Result<Report> {
    let WorkInput {
        path,
        plan,
        original,
        output,
        paths,
    } = input;
    workspace.check()?;
    super::super::plan::absolute(output)?;
    let target = c
        .owner
        .as_ref()
        .context("target owner missing")?
        .path
        .clone();
    let target_witness = large::Source::inspect(&target)?;
    ensure!(
        target == paths.target,
        "target changed after path qualification"
    );
    let source = &paths.source;
    let output_parent = output.parent().context("report parent")?.canonicalize()?;
    files::private_directory(&output_parent)?;
    ensure!(
        !target.starts_with(source)
            && !output_parent.starts_with(source)
            && !target.starts_with(output),
        "recovery paths overlap"
    );
    workspace.probe(&output_parent, 4096)?;
    files::create_directory(output)?;
    let key: [u8; 32] = files::read(&source.join("inventory.key"), 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("inventory key width"))?;
    let mut forbidden = vec![target.clone(), source.clone(), output.to_path_buf()];
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = target.as_os_str().to_os_string();
        name.push(suffix);
        forbidden.push(PathBuf::from(name));
    }
    let plan_path = path.canonicalize()?;
    ensure!(
        !forbidden
            .iter()
            .any(|other| plan_path == *other || plan_path.starts_with(other)),
        "plan overlaps database/archive/output"
    );
    forbidden.push(plan_path.clone());
    let mut provisioning = Prepared::open(&plan.target.provisioning, &forbidden, workspace)?;
    forbidden.push(provisioning.path().to_path_buf());
    let mut approval = Prepared::open(&plan.target.static_manifest, &forbidden, workspace)?;
    forbidden.push(approval.path().to_path_buf());
    let mut retired = Prepared::open(&plan.retirement, &forbidden, workspace)?;
    forbidden.push(retired.path().to_path_buf());
    let mut producer = Prepared::open(&plan.source.producer.evidence, &forbidden, workspace)?;
    forbidden.push(producer.path().to_path_buf());
    // Includes the canonical owner-lock name and any existing lock alias from
    // pre-write qualification, as well as all immutable control inputs.
    forbidden.extend(paths.forbidden.iter().cloned());
    let (attestation, producer_bytes, producer_charge) =
        inputs::read_control::<Producer>(producer.path(), workspace)?;
    ensure!(
        attestation.format == "bitrouter.offline.source-producer-attestation.v1"
            && attestation.installation_id == plan.source.installation_id
            && attestation.commit == plan.source.producer.commit
            && attestation.artifact_sha256 == plan.source.producer.artifact_sha256
            && attestation.database_evidence_digest == plan.source.evidence_digest
            && files::sha(&producer_bytes) == plan.source.producer.evidence.sha256,
        "producer attestation binding mismatch"
    );
    workspace.probe(output, u64::try_from(original.len())?)?;
    files::write_new(&output.join("plan.json"), original)?;
    for (input, name) in [
        (&provisioning, "provisioning.jsonl"),
        (&approval, "static-approval.jsonl"),
        (&retired, "retirement.jsonl"),
        (&producer, "producer.json"),
    ] {
        large::copy_new(input.path(), &output.join(name), workspace)?;
    }
    drop(producer_bytes);
    drop(producer_charge);
    drop(attestation);
    let selected_files = retirement::inspect(
        &mut retired,
        &plan.source,
        output,
        &forbidden,
        &key,
        workspace,
        true,
    )?;
    let target_digest = large::snapshot_digest(&target, &key, workspace)?;
    let (verified, _verified_charge, _ready) = crate::offline::streamed::read_borrowed(
        source,
        Some(&plan.source.evidence_digest),
        "recover",
        c,
        workspace,
    )
    .await?;
    super::super::catalog::dispositions(&plan.source, &verified.evidence.inventory)?;
    workspace.check()?;
    c.open_immutable_source_budgeted(&target, workspace).await?;
    c.uncertain = true;
    sea_orm::sqlx::query("BEGIN")
        .execute(c.source.as_mut().context("target connection")?)
        .await?;
    let target_result = async {
        let db = c.source.as_mut().context("target connection")?;
        let inspected = large::inspect_in(db, &key, workspace).await?;
        ensure!(
            inspected.value().schema_digest == verified.evidence.inventory.schema_digest,
            "fresh target schema differs from source producer schema"
        );
        let proof = crate::tenancy::audit::recovery::v2::inspect_target_in(
            db,
            inspected.value(),
            &plan.target,
            &mut provisioning,
            &mut approval,
            validation,
            workspace,
        )
        .await?;
        Ok::<_, anyhow::Error>((inspected, proof))
    }
    .await;
    c.destination_cleanup_budget().await?;
    sea_orm::sqlx::query("ROLLBACK")
        .execute(c.source.as_mut().context("target rollback connection")?)
        .await?;
    c.uncertain = false;
    c.close().await?;
    let (target_inventory, mut proof) = target_result?;
    target_witness.check()?;
    ensure!(
        large::snapshot_digest(&target, &key, workspace)? == target_digest,
        "target changed during verification"
    );
    workspace.check()?;
    c.open_budgeted(&source.join("snapshot.sqlite"), true, workspace)
        .await?;
    c.uncertain = true;
    sea_orm::sqlx::query("BEGIN")
        .execute(c.copy.as_mut().context("source connection")?)
        .await?;
    let source_result = crate::tenancy::audit::recovery::v2::no_copied_credentials_in(
        c.copy.as_mut().context("source connection")?,
        &plan.source.installation_id,
        &mut proof.credentials,
        validation,
        workspace,
    )
    .await;
    c.cleanup_budget().await?;
    sea_orm::sqlx::query("ROLLBACK")
        .execute(c.copy.as_mut().context("source rollback connection")?)
        .await?;
    c.uncertain = false;
    c.close().await?;
    let source_facts = source_result?;
    drop(proof.credentials);
    // Reverify the bundle after all original SQL workers have acknowledged close.
    let (after, _after_charge, _after_ready) = crate::offline::streamed::read_borrowed(
        source,
        Some(&plan.source.evidence_digest),
        "recover",
        c,
        workspace,
    )
    .await?;
    ensure!(
        after.evidence.inventory == verified.evidence.inventory
            && after.evidence.snapshot_digest == verified.evidence.snapshot_digest,
        "source archive changed"
    );
    for input in [
        &mut provisioning,
        &mut approval,
        &mut retired,
        &mut producer,
    ] {
        input.recheck(workspace)?;
    }
    for (input, name) in [
        (&provisioning, "provisioning.jsonl"),
        (&approval, "static-approval.jsonl"),
        (&retired, "retirement.jsonl"),
        (&producer, "producer.json"),
    ] {
        input.verify_copy(&output.join(name), workspace)?;
    }
    let (_, final_plan, _final_charge) = inputs::read_control::<Plan>(path, workspace)?;
    ensure!(final_plan == original, "plan changed during verification");
    ensure!(
        retirement::inspect(
            &mut retired,
            &plan.source,
            output,
            &forbidden,
            &key,
            workspace,
            false
        )? == selected_files,
        "retirement selection changed"
    );
    target_witness.check()?;
    ensure!(
        large::snapshot_digest(&target, &key, workspace)? == target_digest,
        "target changed before publication"
    );
    let inventory = target_inventory.value();
    workspace.check()?;
    Ok(Report {
        format: "bitrouter.offline.recovery-evidence.v2",
        action: "recover",
        plan_digest: files::sha(original),
        database_evidence_digest: plan.source.evidence_digest.clone(),
        source: source_facts,
        target: proof.facts,
        target_rows: inventory.rows,
        target_value_bytes: inventory.value_bytes,
        target_schema_digest: inventory.schema_digest.clone(),
        target_file_digest: target_digest.1,
        selected_files,
        source_producer_attestation_bound: true,
        operational_fence_attestation_bound: true,
        external_writers_independently_excluded: false,
        fresh_provisioning_verified: true,
        static_closure_authenticated: true,
        history_transferred: false,
        upstream_effects_resolved: false,
        complete: false,
        cutover_ready: false,
    })
}
