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
};
use tokio_util::sync::CancellationToken;

pub(super) async fn backfill(
    plan: &Path,
    plan_digest: &str,
    report: &Path,
) -> Result<impl crate::output::CliReport> {
    apply::run(plan, plan_digest, report).await
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
    ensure!(cfg!(unix), "Unix only");
    ensure!(
        matches!(source_kind, "local_user" | "shared_user" | "native_owner"),
        "explicit source owner namespace required"
    );
    files::validate_digest(anchor)?;
    let verified = super::read_bundle(bundle, Some(anchor), "eval-inventory").await?;
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
    let task = tokio::spawn(async move {
        let mut connections = Connections::new(cancel.clone(), None);
        let body = AssertUnwindSafe(inspect(
            &mut connections,
            &source,
            &key,
            &installation,
            &source_kind,
            &verified.evidence,
        ))
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
        let result = if !matches!(close, Ok(Ok(()))) || connections.uncertain {
            Err(anyhow::anyhow!("Eval cleanup unconfirmed"))
        } else {
            match body {
                Ok(graph) => write_inventory(
                    &source,
                    &destination,
                    &verified,
                    &installation,
                    &source_kind,
                    &graph,
                    &cancel,
                ),
                Err(error) => Err(error),
            }
        };
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
async fn inspect(
    connections: &mut Connections,
    bundle: &Path,
    key: &[u8; 32],
    installation: &InstallationId,
    kind: &str,
    evidence: &Evidence,
) -> Result<Graph> {
    connections
        .open(&bundle.join("snapshot.sqlite"), true)
        .await?;
    connections.uncertain = true;
    let db = connections
        .copy
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("source connection missing"))?;
    sea_orm::sqlx::query("BEGIN").execute(&mut *db).await?;
    let body = async {
        let tables = rows::load(db, key).await?;
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
        Graph::decode(tables, key, installation, kind)
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
        files::snapshot_digest(&bundle.join("snapshot.sqlite"), key)?
            == (evidence.snapshot_bytes, evidence.snapshot_digest.clone()),
        "source bundle changed"
    );
    connections.check()?;
    Ok(graph)
}
fn write_inventory(
    bundle: &Path,
    output: &Path,
    verified: &super::format::Report,
    installation: &InstallationId,
    kind: &str,
    graph: &Graph,
    cancel: &CancellationToken,
) -> Result<Summary> {
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
        format: "bitrouter.offline.eval-partition.v1",
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
    let bytes = serde_json::to_vec_pretty(&packet)?;
    ensure!(bytes.len() <= 2 * 1024 * 1024, "packet bound");
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
    files::create_directory(output)?;
    files::write_new(&output.join("partitions.json"), &bytes)?;
    files::sync_directory(output)?;
    files::write_new(
        &output.join("RECORDED"),
        format!("{}\n", summary.packet_digest).as_bytes(),
    )?;
    files::sync_directory(output)?;
    std::fs::remove_file(output.join("INCOMPLETE"))?;
    files::sync_directory(output)?;
    files::sync_directory(&parent)?;
    Ok(summary)
}
