//! Private, versioned DB evidence; deliberately not a MigrationManifest.
use serde::{Deserialize, Serialize};

pub(super) const FORMAT: &str = "bitrouter.offline.sqlite-evidence.v1";
pub(super) const EXCLUSIONS: [&str; 5] = [
    "external_oauth_not_fenced_or_included",
    "workspace_state_not_fenced_or_included",
    "external_effect_outcomes_unclassified",
    "producer_build_and_commit_unknown",
    "ownership_fence_covers_participating_binaries_only",
];
pub(super) const MAX_FILE: u64 = 512 * 1024 * 1024;
pub(super) const MAX_EVIDENCE: u64 = 2 * 1024 * 1024;
pub(super) const MAX_ROWS: u64 = 1_000_000;
pub(super) const MAX_BYTES: u64 = 256 * 1024 * 1024;
pub(super) const MAX_CELL: u64 = 16 * 1024 * 1024;
pub(super) const MAX_ROW: u64 = 32 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Column {
    pub name: String,
    pub primary_key_position: i64,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub rows: u64,
    pub value_bytes: u64,
    pub digest: String,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerEntry {
    pub version: String,
    pub applied_at: i64,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Inventory {
    pub encoding: String,
    pub schema_digest: String,
    pub tables: Vec<Table>,
    pub rows: u64,
    pub value_bytes: u64,
    /// None means the ledger table was absent, not an empty ledger.
    pub ledger: Option<Vec<LedgerEntry>>,
    pub installation_id: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Evidence {
    pub format: String,
    pub scope: String,
    pub complete: bool,
    pub cutover_ready: bool,
    pub exclusions: Vec<String>,
    pub inspector: bitrouter_ai::artifact::ArtifactIdentity,
    pub source_commit: Option<String>,
    pub source_build: Option<String>,
    pub selected_path_digest: String,
    pub key_id: String,
    pub snapshot_bytes: u64,
    pub snapshot_digest: String,
    pub inventory: Inventory,
}
impl Evidence {
    pub fn validate_profile(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.format == FORMAT
                && self.scope == "sqlite_database_only"
                && !self.complete
                && !self.cutover_ready
                && self.source_commit.is_none()
                && self.source_build.is_none()
                && self.exclusions == EXCLUSIONS
                && self.snapshot_bytes <= MAX_FILE
                && !self.inspector.version.is_empty()
                && self.inspector.version.len() <= 128
                && self.inspector.target.len() <= 128
                && self.inspector.features.len() <= 128
                && self
                    .inspector
                    .features
                    .iter()
                    .all(|feature| feature.len() <= 128)
                && self
                    .inspector
                    .source_revision
                    .as_ref()
                    .is_none_or(|value| value.len() <= 64),
            "unsupported offline evidence profile"
        );
        for digest in [
            &self.inspector.handoff_build_id,
            &self.selected_path_digest,
            &self.key_id,
            &self.snapshot_digest,
        ] {
            super::files::validate_digest(digest)?;
        }
        Ok(())
    }
}

#[derive(Serialize)]
pub(super) struct Report {
    pub action: &'static str,
    pub evidence_digest: String,
    pub evidence_anchor_checked: bool,
    pub database_inventory_verified: bool,
    pub evidence: Evidence,
}
impl crate::output::CliReport for Report {
    fn render(&self, human: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        human.line("DB-only offline evidence verified; INCOMPLETE for recovery/cutover.")?;
        human.line(&format!(
            "Tables: {}; rows: {}; typed bytes: {}",
            self.evidence.inventory.tables.len(),
            self.evidence.inventory.rows,
            self.evidence.inventory.value_bytes
        ))?;
        human.line(&format!("Evidence SHA256: {}", self.evidence_digest))?;
        human.line("External OAuth/workspace state is not included or fenced. Not a full backup.")
    }
}
