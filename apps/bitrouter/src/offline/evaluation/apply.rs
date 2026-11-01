//! One retained offline owner: validate -> BEGIN -> bounded DML -> verify -> COMMIT.
mod large;
use super::*;
use crate::core::eval::store::migration::{
    Assignment,
    transfer::{Anchors, PROOFS, Transfer},
};
use crate::offline::mapping::schema::{Disposition, Manifest, ResourceKind, SourceKind};
use bitrouter_ai::tenancy::identity::OperationId;
use sea_orm::sqlx::SqliteConnection;
use std::collections::BTreeMap;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    format: String,
    bundle: PathBuf,
    database_evidence_digest: String,
    source_kind: String,
    ownership_evidence_digest: String,
    ownership_evidence: PathBuf,
    manifest: PathBuf,
    manifest_digest: String,
    target: PathBuf,
    target_bundle: PathBuf,
    target_evidence_digest: String,
    target_installation: InstallationId,
    provisioning_operations: Vec<OperationId>,
}
#[derive(serde::Serialize)]
pub(super) struct Report {
    format: &'static str,
    plan_digest: String,
    mapping_digest: String,
    database_evidence_digest: String,
    target_evidence_digest: String,
    target_installation: InstallationId,
    imported_counts: [usize; 4],
    imported_digests: [String; 4],
    provenance_rows: usize,
    quarantined_eval: Vec<crate::core::eval::store::migration::Partition>,
    quarantined_tables: Vec<super::super::format::Table>,
    backfill_applied: bool,
    complete: bool,
    cutover_ready: bool,
}
struct PendingReport {
    report: Report,
    encoded: Option<(Vec<u8>, crate::offline::large::Charge)>,
    _source: Option<crate::offline::large::Charge>,
    _graph: Option<crate::offline::large::Charge>,
}

impl crate::output::CliReport for Report {
    fn render(&self, out: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        out.line("Bounded Legacy Eval backfill committed and verified. Retirement/cutover remain incomplete.")?;
        out.line(&format!(
            "Imported rows: {:?}; snapshot provenance: {}",
            self.imported_counts, self.provenance_rows
        ))
    }
}

pub(super) async fn run(path: &Path, anchor: &str, report: &Path) -> Result<Report> {
    run_inner(
        path,
        anchor,
        report,
        #[cfg(all(test, unix))]
        None,
    )
    .await
}
async fn run_inner(
    path: &Path,
    anchor: &str,
    report: &Path,
    #[cfg(all(test, unix))] test: Option<testing::Schedule>,
) -> Result<Report> {
    run_inner_options(
        path,
        anchor,
        report,
        None,
        #[cfg(all(test, unix))]
        test,
    )
    .await
}

pub(super) async fn run_with_options(
    path: &Path,
    anchor: &str,
    report: &Path,
    options: crate::offline::large::Options,
) -> Result<Report> {
    run_inner_options(
        path,
        anchor,
        report,
        Some(options),
        #[cfg(all(test, unix))]
        None,
    )
    .await
}
async fn run_inner_options(
    path: &Path,
    anchor: &str,
    report: &Path,
    options: Option<crate::offline::large::Options>,
    #[cfg(all(test, unix))] test: Option<testing::Schedule>,
) -> Result<Report> {
    ensure!(cfg!(unix), "Unix only");
    files::validate_digest(anchor)?;
    let cancel = CancellationToken::new();
    let _abandon = cancel.clone().drop_guard();
    if let Some(options) = &options {
        let length = usize::try_from(std::fs::symlink_metadata(path)?.len())?;
        let initial = length
            .checked_mul(6)
            .and_then(|n| n.checked_add(8192))
            .ok_or(crate::offline::large::CapacityExceeded)?;
        ensure!(
            initial <= options.working_bytes,
            crate::offline::large::CapacityExceeded
        );
    }
    let bytes = files::read(path, 2 * 1024 * 1024)?;
    ensure!(files::sha(&bytes) == anchor, "plan anchor mismatch");
    let shape = options
        .as_ref()
        .map(|options| {
            let shape = crate::offline::recovery::v2::memory::json_required(&bytes)?;
            let required = bytes
                .capacity()
                .checked_mul(2)
                .and_then(|n| n.checked_add(shape))
                .and_then(|n| {
                    n.checked_add(options.scratch_directory.as_os_str().len().checked_mul(8)?)
                })
                .and_then(|n| n.checked_add(32768))
                .ok_or(crate::offline::large::CapacityExceeded)?;
            ensure!(
                required <= options.working_bytes,
                crate::offline::large::CapacityExceeded
            );
            Ok::<_, anyhow::Error>(shape)
        })
        .transpose()?;
    let (plan, provisioning) = match &options {
        Some(_) => large::decode(&bytes)?,
        None => (serde_json::from_slice::<Plan>(&bytes)?, None),
    };
    for path in [
        &plan.bundle,
        &plan.manifest,
        &plan.target,
        &plan.target_bundle,
        &plan.ownership_evidence,
    ] {
        ensure!(path.is_absolute(), "explicit absolute input paths required");
    }
    ensure!(
        (plan.format == "bitrouter.offline.eval-backfill.v1"
            && plan.provisioning_operations.len() <= 4096)
            || (plan.format == large::FORMAT && provisioning.is_some()),
        "plan profile"
    );
    for digest in [
        &plan.database_evidence_digest,
        &plan.ownership_evidence_digest,
        &plan.manifest_digest,
        &plan.target_evidence_digest,
    ] {
        files::validate_digest(digest)?;
    }
    ensure!(
        matches!(
            plan.source_kind.as_str(),
            "local_user" | "shared_user" | "native_owner"
        ),
        "explicit source namespace required"
    );
    if let Some(options) = &options {
        large::qualify(&plan, report, options)?;
    }
    let workspace = options
        .as_ref()
        .map(|options| {
            crate::offline::large::Workspace::create(options, cancel.clone()).map(Arc::new)
        })
        .transpose()?;
    let encoded_charge = workspace
        .as_ref()
        .map(|workspace| {
            workspace.reserve(
                bytes
                    .capacity()
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(8192))
                    .ok_or(crate::offline::large::CapacityExceeded)?,
            )
        })
        .transpose()?;
    let plan_charge = workspace
        .as_ref()
        .zip(shape)
        .map(|(workspace, shape)| workspace.reserve(shape))
        .transpose()?;
    let (send, wait) = tokio::sync::oneshot::channel();
    let report = report.to_path_buf();
    let anchor = anchor.to_owned();
    let task = tokio::spawn(async move {
        let _inputs = (encoded_charge, plan_charge);
        let owner = match crate::tenancy::ownership::acquire_path(&plan.target, false) {
            Ok(owner) => owner,
            Err(error) => {
                let _ = send.send(Err(error));
                return;
            }
        };
        let mut connections = Connections::new(cancel.clone(), Some(owner));
        let result = AssertUnwindSafe(async {
            let large = match (&workspace, &options, &provisioning) {
                (Some(workspace), Some(options), Some(provisioning)) => Some(
                    large::Large::prepare(&plan, provisioning, &report, workspace, options)?,
                ),
                _ => None,
            };
            work(
                &mut connections,
                &plan,
                &anchor,
                &bytes,
                &report,
                large,
                #[cfg(all(test, unix))]
                test,
            )
            .await
        })
        .catch_unwind()
        .await;
        let result = match result {
            Ok(value) => value,
            Err(_) => {
                connections.uncertain = true;
                Err(anyhow::anyhow!("backfill owner unwound"))
            }
        };
        let close = AssertUnwindSafe(connections.close()).catch_unwind().await;
        let cleaned = match workspace {
            Some(workspace) => Arc::try_unwrap(workspace)
                .map_err(|_| anyhow::anyhow!("Eval work still owned"))
                .and_then(|workspace| workspace.finish()),
            None => Ok(()),
        };
        let result = if !matches!(close, Ok(Ok(()))) || connections.uncertain {
            Err(anyhow::anyhow!(
                "backfill outcome/cleanup unconfirmed; destination ownership retained"
            ))
        } else {
            match result {
                Ok(value) => (|| {
                    cleaned?;
                    if let Some(options) = &options {
                        crate::offline::large::probe(
                            report.parent().unwrap_or(Path::new(".")),
                            options.min_free_bytes,
                            u64::try_from(
                                value
                                    .encoded
                                    .as_ref()
                                    .map_or(2 * 1024 * 1024, |(bytes, _)| bytes.len()),
                            )?
                            .checked_add(8192)
                            .ok_or(crate::offline::large::CapacityExceeded)?,
                        )?;
                    }
                    publish(
                        &report,
                        &value.report,
                        value.encoded.as_ref().map(|(bytes, _)| bytes.as_slice()),
                        &cancel,
                    )?;
                    Ok(value.report)
                })(),
                Err(error) => Err(error),
            }
        };
        drop(connections);
        if send.send(result).is_err() {
            tracing::debug!("backfill reply abandoned after owned cleanup");
        }
    });
    let result = wait
        .await
        .map_err(|_| anyhow::anyhow!("backfill owner unavailable"))?;
    task.await
        .map_err(|_| anyhow::anyhow!("backfill owner join unavailable"))?;
    result
}

async fn work(
    c: &mut Connections,
    plan: &Plan,
    anchor: &str,
    plan_bytes: &[u8],
    output: &Path,
    mut large: Option<large::Large<'_>>,
    #[cfg(all(test, unix))] mut test: Option<testing::Schedule>,
) -> Result<PendingReport> {
    if large.is_some() {
        c.validation_budget().await?;
    }
    c.check()?;
    let source = match &large {
        Some(large) => {
            large
                .bundle(
                    &plan.bundle,
                    &plan.database_evidence_digest,
                    "eval-backfill",
                )
                .await?
        }
        None => {
            super::super::read_bundle_work(
                &plan.bundle,
                Some(&plan.database_evidence_digest),
                "eval-backfill",
                c,
            )
            .await?
        }
    };
    let _source_report = large
        .as_ref()
        .map(|large| large.workspace.reserve(source.evidence.retained_bytes()?))
        .transpose()?;
    let target = match &large {
        Some(large) => {
            large
                .bundle(
                    &plan.target_bundle,
                    &plan.target_evidence_digest,
                    "eval-target",
                )
                .await?
        }
        None => {
            super::super::read_bundle_work(
                &plan.target_bundle,
                Some(&plan.target_evidence_digest),
                "eval-target",
                c,
            )
            .await?
        }
    };
    let _target_report = large
        .as_ref()
        .map(|large| large.workspace.reserve(target.evidence.retained_bytes()?))
        .transpose()?;
    c.check()?;
    ensure!(
        source.evidence.inventory.encoding == "UTF-8"
            && target.evidence.inventory.encoding == "UTF-8",
        "UTF-8 required"
    );
    let installation: InstallationId = source
        .evidence
        .inventory
        .installation_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("observed source installation required"))?
        .try_into()?;
    ensure!(
        installation != plan.target_installation,
        "source/target installation overlap"
    );
    let key: [u8; 32] = files::read(&plan.bundle.join("inventory.key"), 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("source key width"))?;
    let target_key: [u8; 32] = files::read(&plan.target_bundle.join("inventory.key"), 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("target key width"))?;
    let (graph, _graph_charge) = super::inspect_inner(
        c,
        &plan.bundle,
        &key,
        &installation,
        &plan.source_kind,
        &source.evidence,
        large.as_ref().map(|large| large.workspace),
    )
    .await?;
    let _mapping_input = large
        .as_ref()
        .map(|large| large.input_charge(&plan.manifest, super::super::mapping::schema::MAX_INPUT))
        .transpose()?;
    let mapping_bytes = files::read(&plan.manifest, super::super::mapping::schema::MAX_INPUT)?;
    ensure!(
        files::sha(&mapping_bytes) == plan.manifest_digest,
        "mapping anchor mismatch"
    );
    let _mapping_decode = large
        .as_ref()
        .map(|large| large.decoded_charge(&mapping_bytes))
        .transpose()?;
    let (manifest, _) = Manifest::decode(&mapping_bytes)?;
    ensure!(
        manifest.source_installation_id == installation
            && manifest.source_snapshot_digest.as_str() == source.evidence.snapshot_digest,
        "mapping source mismatch"
    );
    let _ownership = large
        .as_ref()
        .map(|large| large.input_charge(&plan.ownership_evidence, 2 * 1024 * 1024))
        .transpose()?;
    let ownership_bytes = files::read(&plan.ownership_evidence, 2 * 1024 * 1024)?;
    ensure!(
        files::sha(&ownership_bytes) == plan.ownership_evidence_digest,
        "ownership evidence changed"
    );
    let assignments = assign(&graph, &manifest, plan)?;
    let transfer = graph.transfer(
        &plan.target_installation,
        &assignments,
        Anchors {
            source: &installation,
            kind: &plan.source_kind,
            mapping: &plan.manifest_digest,
            database: &plan.database_evidence_digest,
            ownership: &plan.ownership_evidence_digest,
        },
    )?;
    ensure!(
        transfer.tables.iter().map(Vec::len).sum::<usize>() > 0,
        "empty backfill"
    );
    let mut digests = std::array::from_fn(|_| String::new());
    for (i, table) in rows::TABLES.into_iter().enumerate() {
        let mut hashes = transfer.tables[i]
            .iter()
            .map(|r| rows::digest(table, r, &key))
            .collect::<Result<Vec<_>>>()?;
        hashes.sort_unstable();
        let mut digest = files::Fingerprint::new(&key, b"eval-import.target-table.v1")?;
        digest.field(table.name.as_bytes());
        for row in hashes {
            digest.field(&row);
        }
        digests[i] = hex::encode(digest.finish());
    }
    let report = Report {
        format: if large.is_some() {
            "bitrouter.offline.eval-backfill-report.v2"
        } else {
            "bitrouter.offline.eval-backfill-report.v1"
        },
        plan_digest: anchor.into(),
        mapping_digest: plan.manifest_digest.clone(),
        database_evidence_digest: plan.database_evidence_digest.clone(),
        target_evidence_digest: plan.target_evidence_digest.clone(),
        target_installation: plan.target_installation.clone(),
        imported_counts: std::array::from_fn(|i| transfer.tables[i].len()),
        imported_digests: digests,
        provenance_rows: transfer.proofs.len(),
        quarantined_eval: graph
            .partitions()
            .filter(|p| {
                p.owner
                    .as_ref()
                    .is_none_or(|o| !assignments.contains_key(o))
            })
            .cloned()
            .collect(),
        quarantined_tables: source
            .evidence
            .inventory
            .tables
            .into_iter()
            .filter(|t| !rows::TABLES.iter().any(|s| s.name == t.name))
            .collect(),
        backfill_applied: true,
        complete: false,
        cutover_ready: false,
    };
    let encoded = match &large {
        Some(large) => Some(crate::offline::large::encode_json(
            &report,
            large.workspace,
        )?),
        None => {
            ensure!(
                serde_json::to_vec_pretty(&report)?.len() <= 2 * 1024 * 1024,
                "planned report bound"
            );
            None
        }
    };
    let path = c
        .owner
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("target owner missing"))?
        .path
        .clone();
    ensure!(
        !path.starts_with(plan.bundle.canonicalize()?)
            && !path.starts_with(plan.target_bundle.canonicalize()?),
        "destination inside immutable evidence"
    );
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure!(
        !parent
            .canonicalize()?
            .starts_with(plan.bundle.canonicalize()?)
            && !parent
                .canonicalize()?
                .starts_with(plan.target_bundle.canonicalize()?),
        "output inside evidence"
    );
    // A new secured packet exists before any DML. Unknown/cancelled/error outcomes
    // leave INCOMPLETE and never acquire RECORDED by observation or retry.
    if let Some(large) = &large {
        let required = plan_bytes
            .len()
            .checked_add(mapping_bytes.len())
            .and_then(|n| n.checked_add(ownership_bytes.len()))
            .and_then(|n| n.checked_add(8192))
            .ok_or(crate::offline::large::CapacityExceeded)?;
        large.workspace.probe(parent, u64::try_from(required)?)?;
    }
    files::create_directory(output)?;
    files::write_new(&output.join("plan.json"), plan_bytes)?;
    files::write_new(&output.join("manifest.json"), &mapping_bytes)?;
    files::write_new(&output.join("ownership-evidence"), &ownership_bytes)?;
    files::sync_directory(output)?;
    let actual = if let Some(large) = &large {
        let witness = crate::offline::large::Source::inspect(&path)?;
        c.open_immutable_source_budgeted(&path, large.workspace)
            .await?;
        c.uncertain = true;
        sea_orm::sqlx::query("BEGIN")
            .execute(destination(c)?)
            .await?;
        let actual = large::inventory_in(destination(c)?, &target_key, Some(large)).await;
        c.destination_cleanup_budget().await?;
        sea_orm::sqlx::query("ROLLBACK")
            .execute(destination(c)?)
            .await?;
        c.uncertain = false;
        c.close().await?;
        witness.check()?;
        actual?
    } else {
        c.open(&path, false).await?;
        (
            super::super::inventory::inspect_eval(destination(c)?, &target_key).await?,
            None,
        )
    };
    ensure!(
        actual.0 == target.evidence.inventory,
        "target changed since provisioning evidence"
    );
    drop(actual);
    c.close().await?;
    if let Some(large) = &large {
        c.open_destination_budgeted(&path, large.workspace).await?;
    } else {
        c.open_destination(&path).await?;
    }
    c.uncertain = true;
    sea_orm::sqlx::query("BEGIN IMMEDIATE")
        .execute(destination(c)?)
        .await?;
    let body = async {
        #[cfg(all(test, unix))]
        if let Some(schedule) = test.as_mut() {
            schedule.entered().await?;
        }
        c.check()?;
        let (actual, _actual_charge) =
            large::inventory_in(destination(c)?, &target_key, large.as_ref()).await?;
        ensure!(
            actual == target.evidence.inventory,
            "target changed before writer transaction"
        );
        if let Some(large) = &mut large {
            large::validate(destination(c)?, &actual, plan, &assignments, large).await?;
            let retained_bytes = std::fs::metadata(&path)?.len();
            let writes = u64::try_from(transfer.bytes)?
                .checked_mul(4)
                .and_then(|bytes| {
                    bytes.checked_add(
                        u64::try_from(transfer.proofs.len())
                            .ok()?
                            .checked_mul(16384)?,
                    )
                })
                .and_then(|bytes| bytes.checked_add(retained_bytes))
                .ok_or(crate::offline::large::CapacityExceeded)?;
            large.workspace.probe(
                path.parent()
                    .ok_or_else(|| anyhow::anyhow!("target parent missing"))?,
                writes,
            )?;
        } else {
            let operations = plan
                .provisioning_operations
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            super::target::validate(
                destination(c)?,
                &actual,
                &plan.target_installation,
                &assignments,
                &operations,
            )
            .await?;
            let added_rows =
                transfer.tables.iter().map(Vec::len).sum::<usize>() + transfer.proofs.len();
            ensure!(
                actual.rows + added_rows as u64 <= 4096
                    && actual.value_bytes + transfer.bytes as u64 <= 32 * 1024 * 1024,
                "combined destination bound"
            );
        }
        for (table, values) in rows::TABLES.into_iter().zip(&transfer.tables) {
            for row in values {
                c.check()?;
                rows::insert(destination(c)?, table, row).await?;
            }
        }
        for row in &transfer.proofs {
            c.check()?;
            rows::insert(destination(c)?, PROOFS, row).await?;
        }
        verify(c, &transfer, &key).await?;
        let maximum = transfer.tables[2]
            .iter()
            .map(|row| match row.get(3) {
                Some(rows::Cell::Integer(n)) => Ok(*n),
                _ => Err(anyhow::anyhow!("admission sequence type")),
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max();
        let sequences =
            sea_orm::sqlx::query("SELECT name,seq FROM sqlite_sequence WHERE seq<>0 LIMIT 2")
                .fetch_all(destination(c)?)
                .await?;
        use sea_orm::sqlx::Row as _;
        match maximum {
            Some(maximum) => ensure!(
                sequences.len() == 1
                    && sequences[0].try_get::<String, _>("name")? == "eval_admission_events"
                    && sequences[0].try_get::<i64, _>("seq")? == maximum,
                "sequence high-water mismatch"
            ),
            None => ensure!(sequences.is_empty(), "unexpected sequence mutation"),
        }
        let (after, _after_charge) =
            large::inventory_in(destination(c)?, &target_key, large.as_ref()).await?;
        ensure!(
            after.schema_digest == actual.schema_digest
                && after.ledger == actual.ledger
                && after.installation_id == actual.installation_id,
            "import changed schema/ledger/identity"
        );
        for table in &actual.tables {
            if rows::TABLES.iter().any(|t| t.name == table.name)
                || matches!(
                    table.name.as_str(),
                    "eval_import_provenance" | "sqlite_sequence"
                )
            {
                continue;
            }
            ensure!(
                after.tables.iter().find(|t| t.name == table.name) == Some(table),
                "import changed non-Eval state"
            );
        }
        c.check()
    }
    .await;
    if let Err(error) = body {
        // Explicit known rollback may release the file only after worker close.
        c.destination_cleanup_budget().await?;
        sea_orm::sqlx::query("ROLLBACK")
            .execute(destination(c)?)
            .await?;
        c.uncertain = false;
        return Err(error);
    }
    // Consuming COMMIT error is always unknown, even if a later close succeeds.
    // No SQL retry or after-the-fact row count may turn it into known rollback.
    if let Err(error) = c.check() {
        c.destination_cleanup_budget().await?;
        sea_orm::sqlx::query("ROLLBACK")
            .execute(destination(c)?)
            .await?;
        c.uncertain = false;
        return Err(error);
    }
    #[cfg(all(test, unix))]
    if let Some(schedule) = test.as_ref() {
        schedule.before_commit(destination(c)?).await?;
    }
    let committed = sea_orm::sqlx::query("COMMIT")
        .execute(destination(c)?)
        .await;
    #[cfg(all(test, unix))]
    if let Some(schedule) = test.as_ref() {
        schedule.observe_commit(&committed)?;
    }
    committed?;
    c.uncertain = false;
    c.check()?;
    // Confirm durable readback on the same actual worker before report publication.
    verify(c, &transfer, &key).await?;
    if let Some(large) = &mut large {
        c.close().await?;
        large.provisioning.recheck(large.workspace)?;
    }
    Ok(PendingReport {
        report,
        encoded,
        _source: _source_report,
        _graph: _graph_charge,
    })
}
fn destination(c: &mut Connections) -> Result<&mut SqliteConnection> {
    c.source
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("destination unavailable"))
}
async fn verify(c: &mut Connections, transfer: &Transfer, key: &[u8; 32]) -> Result<()> {
    for (table, values) in rows::TABLES.into_iter().zip(&transfer.tables) {
        c.check()?;
        rows::verify(destination(c)?, table, values, key).await?;
    }
    c.check()?;
    rows::verify(destination(c)?, PROOFS, &transfer.proofs, key).await
}
fn assign(graph: &Graph, manifest: &Manifest, plan: &Plan) -> Result<BTreeMap<String, Assignment>> {
    let kind = match plan.source_kind.as_str() {
        "local_user" => SourceKind::LocalUser,
        "shared_user" => SourceKind::SharedUser,
        "native_owner" => SourceKind::NativeOwner,
        _ => anyhow::bail!("namespace"),
    };
    let partitions = graph.partitions().collect::<Vec<_>>();
    ensure!(
        manifest.mappings.0.len() == partitions.len(),
        "mapping does not exhaust exact Eval partitions"
    );
    let mut output = BTreeMap::new();
    for item in &manifest.mappings.0 {
        ensure!(
            item.resource_kind == ResourceKind::Evaluation
                && item.evidence_digest.as_str() == plan.ownership_evidence_digest,
            "unsupported family/evidence anchor"
        );
        let partition = partitions
            .iter()
            .find(|p| p.owner == item.old_owner)
            .ok_or_else(|| anyhow::anyhow!("foreign mapped owner"))?;
        ensure!(
            item.source_kind
                == if item.old_owner.is_some() {
                    kind
                } else {
                    SourceKind::Unresolved
                }
                && item.source_count == partition.source_count
                && item.source_content_digest.as_str() == partition.source_content_digest,
            "partition count/digest/namespace mismatch"
        );
        for example in &item.resource_examples.0 {
            ensure!(
                graph.examples_match(
                    item.old_owner.as_deref(),
                    &example.legacy_resource_id,
                    example.content_digest.as_str()
                ),
                "example does not name exact typed row"
            );
        }
        if item.disposition == Disposition::Mapped {
            let owner = item
                .old_owner
                .clone()
                .ok_or_else(|| anyhow::anyhow!("unresolved owner"))?;
            ensure!(
                output
                    .insert(
                        owner,
                        Assignment {
                            tenant: item
                                .tenant_id
                                .clone()
                                .ok_or_else(|| anyhow::anyhow!("tenant"))?,
                            principal: item
                                .principal_id
                                .clone()
                                .ok_or_else(|| anyhow::anyhow!("principal"))?,
                        }
                    )
                    .is_none(),
                "duplicate assignment"
            );
        }
    }
    Ok(output)
}
fn publish(
    output: &Path,
    report: &Report,
    encoded: Option<&[u8]>,
    cancel: &CancellationToken,
) -> Result<()> {
    ensure!(
        !cancel.is_cancelled(),
        "caller abandoned committed import; packet remains incomplete"
    );
    let bytes = match encoded {
        Some(bytes) => std::borrow::Cow::Borrowed(bytes),
        None => {
            let bytes = serde_json::to_vec_pretty(report)?;
            ensure!(bytes.len() <= 2 * 1024 * 1024, "report bound");
            std::borrow::Cow::Owned(bytes)
        }
    };
    files::write_new(&output.join("report.json"), &bytes)?;
    files::sync_directory(output)?;
    files::write_new(
        &output.join("RECORDED"),
        format!("{}\n", files::sha(&bytes)).as_bytes(),
    )?;
    files::sync_directory(output)?;
    ensure!(!cancel.is_cancelled(), "caller abandoned report");
    std::fs::remove_file(output.join("INCOMPLETE"))?;
    files::sync_directory(output)?;
    files::sync_directory(
        output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )
}

#[cfg(all(test, unix))]
#[path = "apply/testing.rs"]
mod testing;
