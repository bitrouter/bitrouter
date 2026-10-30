//! Executable M0 mapping validation; no backfill or grant is performed here.
pub(super) mod schema;
use super::{files, format::MAX_EVIDENCE};
use anyhow::{Result, ensure};
use std::path::Path;

#[derive(serde::Serialize)]
pub(super) struct Report {
    format: &'static str,
    manifest_digest: String,
    database_evidence_digest: String,
    manifest_structure_valid: bool,
    database_inventory_verified: bool,
    source_installation_observed: bool,
    producer_verified: bool,
    mapping_counts_verified: bool,
    ownership_evidence_verified: bool,
    backfill_applied: bool,
    complete: bool,
    cutover_ready: bool,
    declared: schema::Counts,
}
impl crate::output::CliReport for Report {
    fn render(&self, human: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        human.line(
            "Mapping syntax and snapshot binding verified; no backfill or authority granted.",
        )?;
        human.line(&format!("Manifest SHA256: {}", self.manifest_digest))?;
        human.line(&format!(
            "Declared mapping keys: {}; quarantined keys: {}",
            self.declared.mapping_keys, self.declared.quarantined_keys
        ))?;
        human.line(
            "Ownership evidence and per-owner counts remain unverified. Not ready for cutover.",
        )
    }
}

pub(super) async fn run(
    bundle: &Path,
    evidence_digest: &str,
    manifest: &Path,
    report: &Path,
) -> Result<Report> {
    ensure!(cfg!(unix), "Unix only");
    files::validate_digest(evidence_digest)?;
    let bytes = files::read(manifest, schema::MAX_INPUT)?;
    let (mapping, counts) = schema::Manifest::decode(&bytes)?;
    let verified = super::read_bundle(bundle, Some(evidence_digest), "mapping").await?;
    ensure!(
        mapping.source_snapshot_digest.as_str() == verified.evidence.snapshot_digest,
        "snapshot binding mismatch"
    );
    if let Some(actual) = &verified.evidence.inventory.installation_id {
        ensure!(
            mapping.source_installation_id.as_str() == actual,
            "installation mismatch"
        );
    }
    // Existing tenant-owned data is not classified into a legacy namespace by
    // this validator. It records exactly the operator's submitted claims only.
    let result = Report {
        format: "bitrouter.offline.mapping-review.v1",
        manifest_digest: files::sha(&bytes),
        database_evidence_digest: verified.evidence_digest,
        manifest_structure_valid: true,
        database_inventory_verified: true,
        source_installation_observed: verified.evidence.inventory.installation_id.is_some(),
        producer_verified: false,
        mapping_counts_verified: false,
        ownership_evidence_verified: false,
        backfill_applied: false,
        complete: false,
        cutover_ready: false,
        declared: counts,
    };
    let report_bytes = serde_json::to_vec_pretty(&result)?;
    ensure!(
        report_bytes.len() as u64 <= MAX_EVIDENCE,
        "report byte bound"
    );
    // New secured packet outside the exact immutable source bundle. All checks
    // precede creation. A crash before RECORDED leaves INCOMPLETE; RECORDED is
    // merely this packet's digest, never M4a READY or dispatch permission.
    let parent = match report.parent().filter(|v| !v.as_os_str().is_empty()) {
        Some(parent) => parent,
        None => Path::new("."),
    };
    ensure!(
        !parent.canonicalize()?.starts_with(bundle.canonicalize()?),
        "report inside source bundle"
    );
    files::create_directory(report)?;
    files::write_new(&report.join("manifest.json"), &bytes)?;
    files::write_new(&report.join("report.json"), &report_bytes)?;
    files::sync_directory(report)?;
    files::write_new(
        &report.join("RECORDED"),
        format!("{}\n", files::sha(&report_bytes)).as_bytes(),
    )?;
    files::sync_directory(report)?;
    std::fs::remove_file(report.join("INCOMPLETE"))?;
    files::sync_directory(report)?;
    files::sync_directory(parent)?;
    Ok(result)
}

#[cfg(test)]
mod tests;
