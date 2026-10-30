//! All opaque file reads occur before SQLite establishment or after actual close.
use super::{
    super::{files, format},
    plan,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

pub(super) struct Inputs {
    pub plan: plan::Plan,
    pub provisioning: plan::Provisioning,
    pub manifest: Vec<u8>,
    pub plan_bytes: Vec<u8>,
    pub producer: Vec<u8>,
    pub retirement: Vec<u8>,
    pub files: Vec<Opaque>,
    paths: Vec<(PathBuf, u64, String)>,
}
pub(super) struct Opaque {
    pub bytes: Vec<u8>,
    pub record: FileRecord,
}
#[derive(Serialize)]
pub(super) struct FileRecord {
    pub index: usize,
    pub path_digest: String,
    pub bytes: u64,
    pub keyed_digest: String,
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

fn digest(key: &[u8; 32], bytes: &[u8]) -> Result<String> {
    let mut value = files::Fingerprint::new(key, b"recovery-selected-file.v1")?;
    value.field(bytes);
    Ok(hex::encode(value.finish()))
}
fn canonical_file(path: &Path, forbidden: &[PathBuf]) -> Result<PathBuf> {
    plan::absolute(path)?;
    let resolved = path.canonicalize()?;
    ensure!(
        !forbidden
            .iter()
            .any(|p| resolved == *p || resolved.starts_with(p)),
        "recovery input overlaps database/archive"
    );
    Ok(resolved)
}
impl Inputs {
    pub(super) fn read(path: &Path, target: &Path, key: &[u8; 32]) -> Result<Self> {
        plan::absolute(path)?;
        let plan_bytes = files::read(path, format::MAX_EVIDENCE)?;
        let plan: plan::Plan = serde_json::from_slice(&plan_bytes)?;
        plan.validate()?;
        ensure!(
            plan.target.database.canonicalize()? == target,
            "target path changed"
        );
        let source = plan.source.bundle.canonicalize()?;
        let mut forbidden = vec![target.to_path_buf(), source];
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut name = target.as_os_str().to_os_string();
            name.push(suffix);
            forbidden.push(PathBuf::from(name));
        }
        let mut paths = Vec::new();
        let mut seen = BTreeSet::new();
        let plan_path = canonical_file(path, &forbidden)?;
        seen.insert(plan_path.clone());
        paths.push((plan_path, format::MAX_EVIDENCE, files::sha(&plan_bytes)));
        let mut read = |anchor: &plan::Anchor, limit: u64| -> Result<Vec<u8>> {
            let canonical = canonical_file(&anchor.path, &forbidden)?;
            ensure!(seen.insert(canonical.clone()), "reused recovery input");
            let bytes = anchor.read(limit)?;
            paths.push((canonical, limit, anchor.sha256.clone()));
            Ok(bytes)
        };
        let producer = read(&plan.source.producer.evidence, format::MAX_EVIDENCE)?;
        let facts: Producer = serde_json::from_slice(&producer)?;
        ensure!(
            facts.format == "bitrouter.offline.source-producer-attestation.v1"
                && facts.installation_id == plan.source.installation_id
                && facts.commit == plan.source.producer.commit
                && facts.artifact_sha256 == plan.source.producer.artifact_sha256
                && facts.database_evidence_digest == plan.source.evidence_digest,
            "source producer attestation does not bind this archive"
        );
        let retirement = read(&plan.retirement.evidence, format::MAX_EVIDENCE)?;
        let actual_retirement: serde_json::Value = serde_json::from_slice(&retirement)?;
        let expected_retirement = serde_json::json!({"format":"bitrouter.offline.retirement-attestation.v1", "source_installation_id":plan.source.installation_id,"database_evidence_digest":plan.source.evidence_digest,"fence_maintained":true,"target_ingress_fenced":true,"selection_exhaustive":true,"unresolved_effects_remain_unknown":true,"writers":plan.retirement.writers,"selected_files":plan.selected_files});
        ensure!(
            actual_retirement == expected_retirement,
            "retirement evidence does not bind exact writers/selections/archive"
        );
        let provisioning_bytes = read(&plan.target.provisioning, format::MAX_EVIDENCE)?;
        let provisioning: plan::Provisioning = serde_json::from_slice(&provisioning_bytes)?;
        provisioning.validate(&plan.target.installation_id)?;
        let manifest = read(&plan.target.static_manifest, 4 * 1024 * 1024)?;
        let mut selected = Vec::new();
        for (index, file) in plan.selected_files.iter().enumerate() {
            let canonical = canonical_file(&file.path, &forbidden)?;
            ensure!(
                seen.insert(canonical.clone()),
                "duplicate selected physical path"
            );
            let bytes = files::read(&file.path, file.bytes)?;
            ensure!(
                bytes.len() as u64 == file.bytes && digest(key, &bytes)? == file.keyed_digest,
                "selected file content changed"
            );
            // Private opaque copies are never parsed, transferred to the target,
            // or treated as evidence that a remote refresh/effect completed.
            let mut path_digest = files::Fingerprint::new(key, b"recovery-selected-path.v1")?;
            path_digest.field(
                canonical
                    .to_str()
                    .context("selected path UTF-8")?
                    .as_bytes(),
            );
            paths.push((canonical, file.bytes, files::sha(&bytes)));
            selected.push(Opaque {
                bytes,
                record: FileRecord {
                    index,
                    path_digest: hex::encode(path_digest.finish()),
                    bytes: file.bytes,
                    keyed_digest: file.keyed_digest.clone(),
                },
            });
        }
        Ok(Self {
            plan,
            provisioning,
            manifest,
            plan_bytes,
            producer,
            retirement,
            files: selected,
            paths,
        })
    }
    /// Caller has received the real worker close ACK before this file I/O.
    pub(super) fn recheck(&self, check: &dyn Fn() -> Result<()>) -> Result<()> {
        for (path, limit, expected) in &self.paths {
            check()?;
            ensure!(
                files::sha(&files::read(path, *limit)?) == *expected,
                "recovery input changed before publication"
            );
        }
        check()
    }
    pub(super) fn write(&self, output: &Path) -> Result<()> {
        files::write_new(&output.join("plan.json"), &self.plan_bytes)?;
        files::write_new(&output.join("producer-attestation.json"), &self.producer)?;
        files::write_new(&output.join("retirement-evidence"), &self.retirement)?;
        for file in &self.files {
            files::write_new(
                &output.join(format!("selected-{:03}.opaque", file.record.index)),
                &file.bytes,
            )?;
        }
        Ok(())
    }
}
