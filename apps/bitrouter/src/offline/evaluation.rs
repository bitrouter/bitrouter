//! Explicit Legacy Eval partition and one offline conversion owner.
mod apply;
pub(crate) mod rows;
mod target;
#[cfg(all(test, unix))]
mod tests;
use super::{
    engine::Connections,
    files,
    format::{Evidence, Table},
};
use crate::core::eval::store::migration::Graph;
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::identity::InstallationId;
use futures::FutureExt;
use std::{
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

pub(super) async fn backfill(
    plan: &Path,
    plan_digest: &str,
    report: &Path,
) -> Result<impl crate::output::CliReport> {
    apply::run(plan, plan_digest, report).await
}

pub(super) async fn backfill_with_options(
    plan: &Path,
    plan_digest: &str,
    report: &Path,
    options: super::large::Options,
) -> Result<impl crate::output::CliReport> {
    apply::run_with_options(plan, plan_digest, report, options).await
}

#[derive(serde::Serialize)]
pub(super) struct Summary {
    format: &'static str,
    packet_digest: String,
    eval_rows: usize,
    eval_typed_bytes: usize,
    owner_partitions: usize,
    externally_quarantined_tables: usize,
    complete: bool,
    cutover_ready: bool,
}
impl crate::output::CliReport for Summary {
    fn render(&self, h: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        h.line(
            "Legacy Eval rows partitioned and closure checked. No backfill or cutover performed.",
        )?;
        h.line(&format!(
            "Rows: {}; owner partitions: {}; external quarantine tables: {}",
            self.eval_rows, self.owner_partitions, self.externally_quarantined_tables
        ))?;
        h.line(&format!("Private packet SHA256: {}", self.packet_digest))
    }
}
#[derive(serde::Serialize)]
struct InventoryPacket<'a> {
    format: &'static str,
    source_installation_id: &'a InstallationId,
    source_kind: &'a str,
    database_evidence_digest: &'a str,
    source_snapshot_digest: &'a str,
    row_encoding: &'static str,
    partitions: Vec<&'a crate::core::eval::store::migration::Partition>,
    external_quarantine: Vec<&'a Table>,
    exclusions: &'a [String],
    source_producer_verified: bool,
    complete: bool,
    cutover_ready: bool,
}

pub(crate) fn partition_digest(
    key: &[u8; 32],
    installation: &InstallationId,
    kind: &str,
    owner: Option<&str>,
    mut hashes: Vec<[u8; 32]>,
) -> Result<String> {
    hashes.sort_unstable();
    let mut digest = files::Fingerprint::new(key, b"eval-partition.owner.v1")?;
    digest.field(installation.as_str().as_bytes());
    digest.field(if owner.is_some() {
        kind.as_bytes()
    } else {
        b"unresolved"
    });
    digest.field(owner.map_or(&[][..], str::as_bytes));
    digest.field(b"evaluation");
    digest.field(&(hashes.len() as u64).to_be_bytes());
    for hash in hashes {
        digest.field(&hash);
    }
    Ok(hex::encode(digest.finish()))
}

pub(super) async fn inventory(
    bundle: &Path,
    anchor: &str,
    source_kind: &str,
    output: &Path,
) -> Result<Summary> {
    inventory_inner(bundle, anchor, source_kind, output, None).await
}

pub(super) async fn inventory_with_options(
    bundle: &Path,
    anchor: &str,
    source_kind: &str,
    output: &Path,
    options: super::large::Options,
) -> Result<Summary> {
    inventory_inner(bundle, anchor, source_kind, output, Some(options)).await
}

async fn inventory_inner(
    bundle: &Path,
    anchor: &str,
    source_kind: &str,
    output: &Path,
    options: Option<super::large::Options>,
) -> Result<Summary> {
    ensure!(cfg!(unix), "Unix only");
    ensure!(
        matches!(source_kind, "local_user" | "shared_user" | "native_owner"),
        "explicit source owner namespace required"
    );
    files::validate_digest(anchor)?;
    if let Some(options) = &options {
        files::private_directory(bundle)?;
        let input = bundle.canonicalize()?;
        ensure!(
            !options
                .scratch_directory
                .canonicalize()?
                .starts_with(&input)
                && !output
                    .parent()
                    .unwrap_or(Path::new("."))
                    .canonicalize()?
                    .starts_with(&input),
            "Eval scratch/output overlaps immutable input"
        );
    }
    let verified = match &options {
        Some(options) => {
            let read_options = super::large::Options {
                working_bytes: options
                    .working_bytes
                    .checked_sub(65536)
                    .ok_or(super::large::CapacityExceeded)?,
                ..options.clone()
            };
            super::read_bundle_with_options(bundle, Some(anchor), "eval-inventory", &read_options)
                .await?
        }
        None => super::read_bundle(bundle, Some(anchor), "eval-inventory").await?,
    };
    let installation: InstallationId = verified
        .evidence
        .inventory
        .installation_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("observed source installation required by this profile"))?
        .try_into()?;
    ensure!(
        verified.evidence.inventory.encoding == "UTF-8",
        "Eval conversion requires UTF-8 typed text"
    );
    let key: [u8; 32] = files::read(&bundle.join("inventory.key"), 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("key width"))?;
    let cancel = CancellationToken::new();
    let _abandon = cancel.clone().drop_guard();
    let (send, wait) = tokio::sync::oneshot::channel();
    let source = bundle.canonicalize()?;
    let destination = output.to_path_buf();
    let source_kind = source_kind.to_owned();
    let workspace = options
        .as_ref()
        .map(|options| super::large::Workspace::create(options, cancel.clone()).map(Arc::new))
        .transpose()?;
    let metadata = workspace
        .as_ref()
        .map(|workspace| {
            workspace.reserve(
                verified
                    .evidence
                    .retained_bytes()?
                    .checked_add(65536)
                    .ok_or(super::large::CapacityExceeded)?,
            )
        })
        .transpose()?;
    let task = tokio::spawn(async move {
        let mut connections = Connections::new(cancel.clone(), None);
        let body = AssertUnwindSafe(async {
            if workspace.is_some() {
                connections.validation_budget().await?;
            }
            inspect_inner(
                &mut connections,
                &source,
                &key,
                &installation,
                &source_kind,
                &verified.evidence,
                workspace.as_ref(),
            )
            .await
        })
        .catch_unwind()
        .await;
        let body = match body {
            Ok(value) => value,
            Err(_) => {
                connections.uncertain = true;
                Err(anyhow::anyhow!("Eval inventory unwound"))
            }
        };
        let close = AssertUnwindSafe(connections.close()).catch_unwind().await;
        let prepared = if matches!(close, Ok(Ok(()))) && !connections.uncertain {
            body.and_then(|(graph, charge)| {
                prepare_inventory(
                    &source,
                    &destination,
                    &verified,
                    InventorySource {
                        installation: &installation,
                        kind: &source_kind,
                    },
                    &graph,
                    &cancel,
                    workspace.as_deref(),
                )
                .map(|value| (value, charge))
            })
        } else {
            Err(anyhow::anyhow!("Eval cleanup unconfirmed"))
        };
        let cleaned = match workspace {
            Some(workspace) => Arc::try_unwrap(workspace)
                .map_err(|_| anyhow::anyhow!("Eval work still owned"))
                .and_then(|workspace| workspace.finish()),
            None => Ok(()),
        };
        let result = prepared.and_then(|((summary, bytes, _output_charge), _graph_charge)| {
            cleaned?;
            if let Some(options) = &options {
                super::large::probe(
                    destination.parent().unwrap_or(Path::new(".")),
                    options.min_free_bytes,
                    u64::try_from(bytes.len())?
                        .checked_add(8192)
                        .ok_or(super::large::CapacityExceeded)?,
                )?;
            }
            ensure!(!cancel.is_cancelled(), "caller abandoned inventory");
            publish_inventory(&destination, &summary, &bytes)?;
            Ok(summary)
        });
        drop(metadata);
        drop(connections);
        if send.send(result).is_err() {
            tracing::debug!("offline Eval reply abandoned after cleanup");
        }
    });
    let result = wait
        .await
        .map_err(|_| anyhow::anyhow!("Eval owner unavailable"))?;
    task.await
        .map_err(|_| anyhow::anyhow!("Eval owner join failed"))?;
    result
}
async fn inspect_inner(
    connections: &mut Connections,
    bundle: &Path,
    key: &[u8; 32],
    installation: &InstallationId,
    kind: &str,
    evidence: &Evidence,
    workspace: Option<&Arc<super::large::Workspace>>,
) -> Result<(Graph, Option<super::large::Charge>)> {
    let _ready = if workspace.is_some() {
        let ready = files::private_file(&bundle.join("READY"), 65)?;
        ready.try_lock_shared()?;
        Some(ready)
    } else {
        None
    };
    if let Some(workspace) = workspace {
        connections
            .open_budgeted(&bundle.join("snapshot.sqlite"), true, workspace)
            .await?;
    } else {
        connections
            .open(&bundle.join("snapshot.sqlite"), true)
            .await?;
    }
    connections.uncertain = true;
    let db = connections
        .copy
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("source connection missing"))?;
    sea_orm::sqlx::query("BEGIN").execute(&mut *db).await?;
    let body = async {
        let mut charge = workspace
            .map(|workspace| reserve_graph(evidence, workspace))
            .transpose()?;
        let tables = rows::load(db, key).await?;
        if let Some(charge) = &mut charge {
            // Raw strings are already bounded and charged. Count decoded JSON
            // shape before the original graph decoder materializes owned trees.
            for (table, rows) in rows::TABLES.into_iter().zip(&tables.rows) {
                for row in rows {
                    for (index, field) in table.fields.iter().enumerate() {
                        if field.name.ends_with("_json") {
                            charge.grow(super::recovery::v2::memory::json_required(
                                row.text(index)?.as_bytes(),
                            )?)?;
                        }
                    }
                }
            }
        }
        for (table, actual) in rows::TABLES.iter().zip(&tables.rows) {
            let recorded = evidence
                .inventory
                .tables
                .iter()
                .find(|t| t.name == table.name)
                .ok_or_else(|| anyhow::anyhow!("source Eval table absent"))?;
            ensure!(
                recorded.rows == actual.len() as u64,
                "source typed count changed"
            );
        }
        Ok::<_, anyhow::Error>((Graph::decode(tables, key, installation, kind)?, charge))
    }
    .await;
    connections.cleanup_budget().await?;
    let db = connections
        .copy
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("source connection missing"))?;
    sea_orm::sqlx::query("ROLLBACK").execute(&mut *db).await?;
    connections.uncertain = false;
    let graph = body?;
    // No source file descriptor is opened while SQLite retains that file.
    connections.close().await?;
    ensure!(
        snapshot_digest(
            &bundle.join("snapshot.sqlite"),
            key,
            workspace,
            connections.owner.clone()
        )
        .await?
            == (evidence.snapshot_bytes, evidence.snapshot_digest.clone()),
        "source bundle changed"
    );
    connections.check()?;
    Ok(graph)
}
struct InventorySource<'a> {
    installation: &'a InstallationId,
    kind: &'a str,
}

fn prepare_inventory(
    bundle: &Path,
    output: &Path,
    verified: &super::format::Report,
    source: InventorySource<'_>,
    graph: &Graph,
    cancel: &CancellationToken,
    workspace: Option<&super::large::Workspace>,
) -> Result<(Summary, Vec<u8>, Option<super::large::Charge>)> {
    let InventorySource { installation, kind } = source;
    let evidence = &verified.evidence;
    ensure!(!cancel.is_cancelled(), "caller abandoned inventory");
    let parent: PathBuf = match output.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(p) => p.into(),
        None => PathBuf::from("."),
    };
    ensure!(
        !parent.canonicalize()?.starts_with(bundle),
        "output inside immutable source"
    );
    let quarantine = evidence
        .inventory
        .tables
        .iter()
        .filter(|t| !rows::TABLES.iter().any(|s| s.name == t.name))
        .collect::<Vec<_>>();
    let packet = InventoryPacket {
        format: if workspace.is_some() {
            "bitrouter.offline.eval-partition.v2"
        } else {
            "bitrouter.offline.eval-partition.v1"
        },
        source_installation_id: installation,
        source_kind: kind,
        database_evidence_digest: &verified.evidence_digest,
        source_snapshot_digest: &evidence.snapshot_digest,
        row_encoding: "eval-partition.row.v1:fixed-named-columns/null/integer-be64/utf8-text",
        partitions: graph.partitions().collect(),
        external_quarantine: quarantine,
        exclusions: &evidence.exclusions,
        source_producer_verified: false,
        complete: false,
        cutover_ready: false,
    };
    let (bytes, charge) = match workspace {
        Some(workspace) => {
            let (bytes, charge) = super::large::encode_json(&packet, workspace)?;
            (bytes, Some(charge))
        }
        None => {
            let bytes = serde_json::to_vec_pretty(&packet)?;
            ensure!(bytes.len() <= 2 * 1024 * 1024, "packet bound");
            (bytes, None)
        }
    };
    let summary = Summary {
        format: packet.format,
        packet_digest: files::sha(&bytes),
        eval_rows: graph.rows,
        eval_typed_bytes: graph.bytes,
        owner_partitions: packet.partitions.len(),
        externally_quarantined_tables: packet.external_quarantine.len(),
        complete: false,
        cutover_ready: false,
    };
    Ok((summary, bytes, charge))
}

fn publish_inventory(output: &Path, summary: &Summary, bytes: &[u8]) -> Result<()> {
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    files::create_directory(output)?;
    files::write_new(&output.join("partitions.json"), bytes)?;
    files::sync_directory(output)?;
    files::write_new(
        &output.join("RECORDED"),
        format!("{}\n", summary.packet_digest).as_bytes(),
    )?;
    files::sync_directory(output)?;
    std::fs::remove_file(output.join("INCOMPLETE"))?;
    files::sync_directory(output)?;
    files::sync_directory(parent)?;
    Ok(())
}

fn reserve_graph(
    evidence: &Evidence,
    workspace: &super::large::Workspace,
) -> Result<super::large::Charge> {
    let (rows, bytes) = rows::TABLES
        .iter()
        .try_fold((0_u64, 0_u64), |(rows, bytes), table| {
            let actual = evidence
                .inventory
                .tables
                .iter()
                .find(|actual| actual.name == table.name)
                .ok_or_else(|| anyhow::anyhow!("source Eval table missing"))?;
            Ok::<_, anyhow::Error>((
                rows.checked_add(actual.rows)
                    .ok_or(super::large::CapacityExceeded)?,
                bytes
                    .checked_add(actual.value_bytes)
                    .ok_or(super::large::CapacityExceeded)?,
            ))
        })?;
    ensure!(
        rows <= rows::MAX_ROWS as u64 && bytes <= rows::MAX_BYTES as u64,
        "Eval graph bound exceeded"
    );
    // Original graph/model strings, transfer copies, row metadata and readback.
    // JSON trees receive an additional shape-based charge before Graph::decode.
    let bytes = usize::try_from(bytes)?
        .checked_mul(16)
        .and_then(|bytes| bytes.checked_add(usize::try_from(rows).ok()?.checked_mul(8192)?))
        .and_then(|bytes| bytes.checked_add(65536))
        .ok_or(super::large::CapacityExceeded)?;
    workspace.reserve(bytes)
}

async fn snapshot_digest(
    path: &Path,
    key: &[u8; 32],
    workspace: Option<&Arc<super::large::Workspace>>,
    owner: Option<crate::tenancy::ownership::Lease>,
) -> Result<(u64, String)> {
    match workspace {
        None => files::snapshot_digest(path, key),
        Some(workspace) => {
            let ready = files::private_file(
                &path
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("bundle parent missing"))?
                    .join("READY"),
                65,
            )?;
            ready.try_lock_shared()?;
            let path = path.to_owned();
            let key = *key;
            let workspace = workspace.clone();
            tokio::task::spawn_blocking(move || {
                let _custody = (ready, owner);
                super::large::snapshot_digest(&path, &key, &workspace)
            })
            .await
            .map_err(|_| anyhow::anyhow!("Eval hash worker failed"))?
        }
    }
}
