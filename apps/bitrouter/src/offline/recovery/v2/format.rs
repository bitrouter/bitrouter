//! Strict v2 control objects and individual JSON-lines records; never a catalog Vec.

use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::{ConfigurationRevision, ResourceRef},
    identity::{AuthorityRevision, InstallationId, PrincipalId, RevisionId, TenantId},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::super::plan::{self, Anchor, Source, Target};

pub(crate) const PLAN: &str = "bitrouter.offline.recovery-plan.v2";
pub(crate) const PROVISIONING: &str = "bitrouter.offline.fresh-provisioning.v2";
pub(crate) const APPROVAL: &str = "bitrouter.offline.static-approval.v2";
pub(crate) const RETIREMENT: &str = "bitrouter.offline.retirement-attestation.v2";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub format: String,
    pub source: Source,
    pub target: Target,
    pub retirement: Anchor,
}

impl Plan {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(self.format == PLAN, "unsupported recovery plan format");
        plan::absolute(&self.source.bundle)?;
        plan::absolute(&self.target.database)?;
        ensure!(
            self.source.installation_id != self.target.installation_id,
            "fresh recovery requires a different installation"
        );
        digest(&self.source.evidence_digest)?;
        digest(&self.source.producer.artifact_sha256)?;
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
        let anchors = [
            &self.source.producer.evidence,
            &self.target.provisioning,
            &self.target.static_manifest,
            &self.retirement,
        ];
        for (index, anchor) in anchors.iter().enumerate() {
            plan::absolute(&anchor.path)?;
            digest(&anchor.sha256)?;
            ensure!(
                anchors[..index].iter().all(|old| old.path != anchor.path),
                "reused recovery anchor path"
            );
        }
        ensure!(
            !self.source.tables.is_empty() && self.source.tables.len() <= 256,
            "source schema table count"
        );
        for (index, table) in self.source.tables.iter().enumerate() {
            label(&table.table)?;
            digest(&table.digest)?;
            ensure!(
                self.source.tables[..index]
                    .iter()
                    .all(|old| old.table != table.table),
                "duplicate table disposition"
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

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProvisioningHeader {
    pub format: String,
    pub installation_id: InstallationId,
    pub operation_count: u64,
    pub configuration_count: u64,
    pub active_count: u64,
    pub resource_count: u64,
}

impl ProvisioningHeader {
    pub(crate) fn validate(&self, installation: &InstallationId) -> Result<()> {
        ensure!(
            self.format == PROVISIONING && &self.installation_id == installation,
            "provisioning identity mismatch"
        );
        ensure!(
            self.operation_count > 0 && self.operation_count <= AuthorityRevision::MAX,
            "provisioning authority revision range"
        );
        for count in [
            self.operation_count,
            self.configuration_count,
            self.active_count,
            self.resource_count,
        ] {
            i64::try_from(count)?;
        }
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ProvisioningRecord {
    Operation {
        principal_id: PrincipalId,
        result: bitrouter_ai::tenancy::operation::OperationResult,
    },
    Configuration {
        revision: ConfigurationRevision,
    },
    Active {
        tenant_id: TenantId,
        revision_id: RevisionId,
        authority_revision: AuthorityRevision,
    },
    Resource {
        reference: ResourceRef,
    },
}

impl<'de> Deserialize<'de> for ProvisioningRecord {
    fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> std::result::Result<Self, D::Error> {
        // Serde's internally tagged enum buffers values into Content. That
        // erases the raw JSON token required by the existing integer wire
        // decoder, which checks exact integers without floating-point rounding. Retain this
        // already-budgeted line and decode the selected strict struct directly
        // from JSON instead; no Value conversion or number normalization.
        let raw = Box::<serde_json::value::RawValue>::deserialize(decoder)?;
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Kind {
            Operation,
            Configuration,
            Active,
            Resource,
        }
        #[derive(Deserialize)]
        struct Tag {
            kind: Kind,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Operation {
            #[serde(rename = "kind")]
            _kind: Kind,
            principal_id: PrincipalId,
            result: bitrouter_ai::tenancy::operation::OperationResult,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Configuration {
            #[serde(rename = "kind")]
            _kind: Kind,
            revision: ConfigurationRevision,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Active {
            #[serde(rename = "kind")]
            _kind: Kind,
            tenant_id: TenantId,
            revision_id: RevisionId,
            authority_revision: AuthorityRevision,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Resource {
            #[serde(rename = "kind")]
            _kind: Kind,
            reference: ResourceRef,
        }
        let tag: Tag = serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
        Ok(match tag.kind {
            Kind::Operation => {
                let value: Operation =
                    serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
                Self::Operation {
                    principal_id: value.principal_id,
                    result: value.result,
                }
            }
            Kind::Configuration => {
                let value: Configuration =
                    serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
                Self::Configuration {
                    revision: value.revision,
                }
            }
            Kind::Active => {
                let value: Active =
                    serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
                Self::Active {
                    tenant_id: value.tenant_id,
                    revision_id: value.revision_id,
                    authority_revision: value.authority_revision,
                }
            }
            Kind::Resource => {
                let value: Resource =
                    serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
                Self::Resource {
                    reference: value.reference,
                }
            }
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApprovalHeader {
    pub format: String,
    pub installation_id: InstallationId,
    pub resource_count: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetirementHeader {
    pub format: String,
    pub source_installation_id: InstallationId,
    pub database_evidence_digest: String,
    pub fence_maintained: bool,
    pub target_ingress_fenced: bool,
    pub selection_exhaustive: bool,
    pub unresolved_effects_remain_unknown: bool,
    pub writer_count: u64,
    pub selected_file_count: u64,
}

impl RetirementHeader {
    pub(crate) fn validate(&self, source: &InstallationId, evidence: &str) -> Result<()> {
        ensure!(
            self.format == RETIREMENT
                && &self.source_installation_id == source
                && self.database_evidence_digest == evidence,
            "retirement evidence binding mismatch"
        );
        digest(&self.database_evidence_digest)?;
        ensure!(
            self.fence_maintained
                && self.target_ingress_fenced
                && self.selection_exhaustive
                && self.unresolved_effects_remain_unknown
                && self.writer_count > 0,
            "retirement fence assertions required"
        );
        self.writer_count
            .checked_add(self.selected_file_count)
            .ok_or_else(|| anyhow::anyhow!("retirement count overflow"))?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RetirementRecord {
    Writer {
        id: String,
        process_identity: String,
        restart_source: String,
        retired: bool,
    },
    File {
        path: PathBuf,
        writer: String,
        keyed_digest: String,
        bytes: u64,
    },
}

pub(crate) fn label(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "invalid recovery label"
    );
    Ok(())
}

pub(crate) fn digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid evidence digest"
    );
    Ok(())
}

/// Zero padding preserves byte-exact SQLite BINARY order for validated IDs.
pub(crate) fn padded<const N: usize>(value: &str) -> Result<[u8; N]> {
    ensure!(
        !value.is_empty() && value.len() <= N && !value.as_bytes().contains(&0),
        "fixed identity width"
    );
    let mut result = [0; N];
    result[..value.len()].copy_from_slice(value.as_bytes());
    Ok(result)
}

pub(crate) fn resource_key(reference: &ResourceRef) -> Result<[u8; 416]> {
    let owner = match reference.owner_kind {
        OwnerKind::Tenant => "tenant",
        OwnerKind::OperatorPool => "operator_pool",
    };
    let kind = match reference.resource_kind {
        ResourceKind::Route => "route",
        ResourceKind::Policy => "policy",
        ResourceKind::Credential => "credential",
        ResourceKind::Checkpoint => "checkpoint",
        ResourceKind::Configuration => "configuration",
        ResourceKind::Execution => "execution",
    };
    let mut key = [0; 416];
    key[..16].copy_from_slice(&padded::<16>(owner)?);
    key[16..144].copy_from_slice(&padded::<128>(reference.owner_id.as_str())?);
    key[144..160].copy_from_slice(&padded::<16>(kind)?);
    key[160..288].copy_from_slice(&padded::<128>(reference.resource_id.as_str())?);
    key[288..].copy_from_slice(&padded::<128>(reference.revision_id.as_str())?);
    Ok(key)
}
