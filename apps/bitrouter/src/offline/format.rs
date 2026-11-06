//! Private, versioned DB evidence; deliberately not a MigrationManifest.
use serde::{Deserialize, Serialize};

pub(super) const FORMAT_V2: &str = "bitrouter.offline.sqlite-evidence.v2";
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
pub(crate) struct Column {
    pub name: String,
    pub primary_key_position: i64,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub rows: u64,
    pub value_bytes: u64,
    pub digest: String,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerEntry {
    pub version: String,
    pub applied_at: i64,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Inventory {
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
    /// Charge an already verified report across subsequent borrowed phases.
    /// Parsing has its own preflight; this counts retained allocation capacities.
    pub(super) fn retained_bytes(&self) -> anyhow::Result<usize> {
        let mut total = std::mem::size_of::<Self>();
        let mut add = |bytes: usize| -> anyhow::Result<()> {
            total = total
                .checked_add(bytes)
                .and_then(|n| n.checked_add(64))
                .ok_or(super::large::CapacityExceeded)?;
            Ok(())
        };
        for value in [
            &self.format,
            &self.scope,
            &self.selected_path_digest,
            &self.key_id,
            &self.snapshot_digest,
            &self.inspector.version,
            &self.inspector.target,
            &self.inspector.handoff_build_id,
            &self.inventory.encoding,
            &self.inventory.schema_digest,
        ] {
            add(value.capacity())?;
        }
        for value in [
            &self.source_commit,
            &self.source_build,
            &self.inspector.source_revision,
            &self.inventory.installation_id,
        ]
        .into_iter()
        .flatten()
        {
            add(value.capacity())?;
        }
        for values in [&self.exclusions, &self.inspector.features] {
            add(values
                .capacity()
                .checked_mul(std::mem::size_of::<String>())
                .ok_or(super::large::CapacityExceeded)?)?;
            for value in values {
                add(value.capacity())?;
            }
        }
        add(self
            .inventory
            .tables
            .capacity()
            .checked_mul(std::mem::size_of::<Table>())
            .ok_or(super::large::CapacityExceeded)?)?;
        for table in &self.inventory.tables {
            add(table.name.capacity())?;
            add(table.digest.capacity())?;
            add(table
                .columns
                .capacity()
                .checked_mul(std::mem::size_of::<Column>())
                .ok_or(super::large::CapacityExceeded)?)?;
            for column in &table.columns {
                add(column.name.capacity())?;
            }
        }
        if let Some(ledger) = &self.inventory.ledger {
            add(ledger
                .capacity()
                .checked_mul(std::mem::size_of::<LedgerEntry>())
                .ok_or(super::large::CapacityExceeded)?)?;
            for entry in ledger {
                add(entry.version.capacity())?;
            }
        }
        total
            .checked_mul(2)
            .and_then(|n| n.checked_add(8192))
            .ok_or_else(|| super::large::CapacityExceeded.into())
    }
    pub fn validate_profile(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.format == FORMAT && self.snapshot_bytes <= MAX_FILE,
            "unsupported offline evidence profile"
        );
        self.validate_metadata()
    }
    pub fn validate_large_profile(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.format == FORMAT_V2,
            "unsupported offline evidence profile"
        );
        self.validate_metadata()
    }
    fn validate_metadata(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.scope == "sqlite_database_only"
                && !self.complete
                && !self.cutover_ready
                && self.source_commit.is_none()
                && self.source_build.is_none()
                && self.exclusions == EXCLUSIONS
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
