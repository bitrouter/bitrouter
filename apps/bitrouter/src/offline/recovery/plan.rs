//! Private operator evidence. None of these fields constructs runtime authority.
use super::super::{files, format};
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{
    configuration::{ConfigurationRevision, ResourceRef},
    identity::{InstallationId, OperationId},
    operation::OperationResult,
    validation::Validate,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

pub(super) const FORMAT: &str = "bitrouter.offline.recovery-plan.v1";
pub(super) const TARGET_ROWS: u64 = 4096;
pub(super) const TARGET_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub format: String,
    pub source: Source,
    pub target: Target,
    pub retirement: Retirement,
    pub selected_files: Vec<SelectedFile>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    pub bundle: PathBuf,
    pub evidence_digest: String,
    pub installation_id: InstallationId,
    pub producer: Producer,
    pub tables: Vec<Disposition>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Producer {
    pub commit: String,
    pub artifact_sha256: String,
    pub evidence: Anchor,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Anchor {
    pub path: PathBuf,
    pub sha256: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Disposition {
    pub table: String,
    pub rows: u64,
    pub value_bytes: u64,
    pub digest: String,
    pub disposition: Archive,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Archive {
    RetainedArchive,
    ExternalQuarantine,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Retirement {
    pub fence_maintained: bool,
    pub target_ingress_fenced: bool,
    pub selection_exhaustive: bool,
    pub unresolved_effects_remain_unknown: bool,
    pub evidence: Anchor,
    pub writers: Vec<Writer>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Writer {
    pub id: String,
    pub process_identity: String,
    pub restart_source: String,
    pub retired: bool,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SelectedFile {
    pub path: PathBuf,
    pub writer: String,
    /// Source-bundle inventory key, domain recovery-selected-file.v1.
    pub keyed_digest: String,
    pub bytes: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Target {
    pub database: PathBuf,
    pub installation_id: InstallationId,
    pub provisioning: Anchor,
    pub wrapping_key_env: String,
    /// Approved NEW static provisioning; no historical-definition transfer.
    pub static_manifest: Anchor,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Provisioning {
    pub format: String,
    pub installation_id: InstallationId,
    pub operations: Vec<OperationObservation>,
    pub configurations: Vec<ConfigurationRevision>,
    pub active: Vec<Active>,
    pub resources: Vec<ResourceRef>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperationObservation {
    pub principal_id: bitrouter_ai::tenancy::identity::PrincipalId,
    pub result: OperationResult,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Active {
    pub tenant: bitrouter_ai::tenancy::identity::TenantId,
    pub revision: bitrouter_ai::tenancy::identity::RevisionId,
    pub authority_revision: bitrouter_ai::tenancy::identity::AuthorityRevision,
}

pub(super) fn absolute(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && path.as_os_str().len() <= 4096,
        "explicit bounded absolute path required"
    );
    ensure!(
        !path.components().any(|c| matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )),
        "path traversal outside recovery profile"
    );
    Ok(())
}
fn label(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "invalid bounded recovery label"
    );
    Ok(())
}
impl Anchor {
    pub(super) fn read(&self, maximum: u64) -> Result<Vec<u8>> {
        absolute(&self.path)?;
        files::validate_digest(&self.sha256)?;
        let bytes = files::read(&self.path, maximum)?;
        ensure!(
            files::sha(&bytes) == self.sha256,
            "recovery evidence anchor mismatch"
        );
        Ok(bytes)
    }
}
impl Plan {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(self.format == FORMAT, "unsupported recovery format");
        absolute(&self.source.bundle)?;
        absolute(&self.target.database)?;
        files::validate_digest(&self.source.evidence_digest)?;
        files::validate_digest(&self.source.producer.artifact_sha256)?;
        ensure!(
            self.source.producer.commit.len() == 40
                && self
                    .source
                    .producer
                    .commit
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "source producer commit required"
        );
        ensure!(
            self.source.installation_id != self.target.installation_id,
            "recovery requires a genuinely different installation"
        );
        let retirement = &self.retirement;
        ensure!(
            retirement.fence_maintained
                && retirement.target_ingress_fenced
                && retirement.selection_exhaustive
                && retirement.unresolved_effects_remain_unknown,
            "maintained operational fence and exhaustive selection required"
        );
        ensure!(
            !retirement.writers.is_empty() && retirement.writers.len() <= 32,
            "retired writer inventory bound"
        );
        let mut writers = BTreeSet::new();
        for writer in &retirement.writers {
            label(&writer.id)?;
            label(&writer.process_identity)?;
            label(&writer.restart_source)?;
            ensure!(
                writer.retired && writers.insert(&writer.id),
                "missing or duplicate retired writer"
            );
        }
        ensure!(self.selected_files.len() <= 128, "selected file bound");
        let mut selected = BTreeSet::new();
        let mut total = 0_u64;
        for file in &self.selected_files {
            absolute(&file.path)?;
            files::validate_digest(&file.keyed_digest)?;
            total = total
                .checked_add(file.bytes)
                .ok_or_else(|| anyhow::anyhow!("selected bytes overflow"))?;
            ensure!(
                file.bytes <= format::MAX_CELL
                    && total <= TARGET_BYTES
                    && writers.contains(&file.writer)
                    && selected.insert(&file.path),
                "selected files outside bounded retired profile"
            );
        }
        ensure!(
            !self.source.tables.is_empty() && self.source.tables.len() <= 256,
            "table disposition bound"
        );
        let mut tables = BTreeSet::new();
        for table in &self.source.tables {
            label(&table.table)?;
            files::validate_digest(&table.digest)?;
            ensure!(
                tables.insert(&table.table)
                    && table.rows <= format::MAX_ROWS
                    && table.value_bytes <= format::MAX_BYTES,
                "invalid table disposition"
            );
        }
        let name = &self.target.wrapping_key_env;
        ensure!(
            !name.is_empty()
                && name.len() <= 128
                && name.bytes().enumerate().all(|(i, b)| b == b'_'
                    || b.is_ascii_alphabetic()
                    || (i > 0 && b.is_ascii_digit())),
            "explicit wrapping key environment reference required"
        );
        Ok(())
    }
}
impl Provisioning {
    pub(super) fn validate(&self, installation: &InstallationId) -> Result<()> {
        ensure!(
            self.format == "bitrouter.offline.fresh-provisioning.v1"
                && &self.installation_id == installation,
            "fresh provisioning identity mismatch"
        );
        ensure!(
            !self.operations.is_empty()
                && self.operations.len() <= TARGET_ROWS as usize
                && !self.configurations.is_empty()
                && self.configurations.len() <= 32
                && !self.active.is_empty()
                && self.active.len() <= 32
                && self.resources.len() <= 256,
            "fresh provisioning bounds"
        );
        let mut ids = BTreeSet::<&OperationId>::new();
        let mut revisions = BTreeSet::new();
        for observed in &self.operations {
            let op = &observed.result;
            op.validate()?;
            let revision = op
                .authority_revision
                .ok_or_else(|| anyhow::anyhow!("unpublished provisioning operation"))?
                .get();
            ensure!(
                op.status == bitrouter_ai::tenancy::operation::OperationStatus::Applied
                    && ids.insert(&op.operation_id)
                    && revision > 0
                    && revision <= self.operations.len() as u64
                    && revisions.insert(revision),
                "non-fresh provisioning operation"
            );
        }
        let mut configs = BTreeSet::new();
        for config in &self.configurations {
            config.validate()?;
            ensure!(
                configs.insert((&config.tenant_id, &config.revision_id)),
                "duplicate configuration readback"
            );
        }
        let mut active = BTreeSet::new();
        for pointer in &self.active {
            ensure!(
                configs.contains(&(&pointer.tenant, &pointer.revision))
                    && active.insert(&pointer.tenant)
                    && pointer.authority_revision.get() <= self.operations.len() as u64,
                "invalid active readback"
            );
        }
        let mut refs = std::collections::HashSet::new();
        for reference in &self.resources {
            ensure!(refs.insert(reference), "duplicate approved resource");
        }
        Ok(())
    }
}
