//! Application-private decoding of frozen M0 MigrationManifest, not authority.
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::identity::{Digest, InstallationId, PrincipalId, TenantId};
use serde::{
    Deserialize, Deserializer,
    de::{SeqAccess, Visitor},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    marker::PhantomData,
};

pub(crate) const MAX_INPUT: u64 = 8 * 1024 * 1024;
const MAX_KEYS: usize = 4096;
const MAX_NUMBER: u64 = 9_007_199_254_740_991;

// Deserialize incrementally: neither a hostile sequence size hint nor an
// oversized array can allocate beyond the local profile. The M0 format allows
// more keys; this command deliberately has a narrower documented work limit.
pub(crate) struct Limited<T, const N: usize>(pub(crate) Vec<T>);
impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for Limited<T, N> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Bounded<T, const N: usize>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for Bounded<T, N> {
            type Value = Limited<T, N>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "an array with at most {N} entries")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while entries.len() < N {
                    match seq.next_element()? {
                        Some(value) => entries.push(value),
                        None => return Ok(Limited(entries)),
                    }
                }
                if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::custom("offline sequence bound exceeded"));
                }
                Ok(Limited(entries))
            }
        }
        deserializer.deserialize_seq(Bounded::<T, N>(PhantomData))
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceKind {
    LocalUser,
    SharedUser,
    NativeOwner,
    Unresolved,
}
#[derive(Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResourceKind {
    Auth,
    Policy,
    Evaluation,
    Evolution,
    Session,
    Trajectory,
    Continuation,
    Receipt,
    Credential,
}
#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Disposition {
    Mapped,
    Quarantined,
}

// M0 requires these fields even when their value is null. Serde's ordinary
// Option would also accept absence, which is not the frozen contract.
fn present_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Example {
    pub(crate) legacy_resource_id: String,
    pub(crate) name: String,
    pub(crate) content_digest: Digest,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Mapping {
    pub(crate) source_installation_id: InstallationId,
    pub(crate) source_kind: SourceKind,
    #[serde(deserialize_with = "present_option")]
    pub(crate) old_owner: Option<String>,
    pub(crate) resource_kind: ResourceKind,
    #[serde(deserialize_with = "present_option")]
    pub(crate) tenant_id: Option<TenantId>,
    #[serde(deserialize_with = "present_option")]
    pub(crate) principal_id: Option<PrincipalId>,
    pub(crate) disposition: Disposition,
    pub(crate) evidence_digest: Digest,
    pub(crate) source_count: u64,
    pub(crate) resource_examples: Limited<Example, 16>,
    pub(crate) rationale: String,
    pub(crate) evidence_ref: String,
    pub(crate) source_content_digest: Digest,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Approval {
    pub(crate) tenant_id: TenantId,
    pub(crate) operator_principal_id: PrincipalId,
    pub(crate) evidence_digest: Digest,
    pub(crate) rationale: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub(crate) schema_version: u64,
    pub source_installation_id: InstallationId,
    pub(crate) mappings: Limited<Mapping, MAX_KEYS>,
    pub(crate) source_commit: String,
    pub source_snapshot_digest: Digest,
    pub(crate) tenant_merge_approvals: Limited<Approval, MAX_KEYS>,
}

#[derive(serde::Serialize)]
pub(crate) struct Counts {
    pub mapping_keys: usize,
    pub declared_mapped_rows: u64,
    pub declared_quarantined_rows: u64,
    pub mapped_keys: usize,
    pub quarantined_keys: usize,
    pub merge_approvals: usize,
}

fn text(value: &str, maximum: usize) -> Result<()> {
    ensure!(
        !value.is_empty() && value.chars().count() <= maximum,
        "text bound"
    );
    Ok(())
}
impl Manifest {
    pub fn decode(bytes: &[u8]) -> Result<(Self, Counts)> {
        ensure!(bytes.len() as u64 <= MAX_INPUT, "manifest byte bound");
        let value: Self = serde_json::from_slice(bytes)?;
        let counts = value.validate()?;
        Ok((value, counts))
    }
    fn validate(&self) -> Result<Counts> {
        ensure!(self.schema_version == 1, "manifest version");
        ensure!(
            matches!(self.source_commit.len(), 40 | 64)
                && self
                    .source_commit
                    .bytes()
                    .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v)),
            "producer commit syntax"
        );
        let mut keys = BTreeSet::new();
        let mut owners: BTreeMap<&TenantId, BTreeSet<(SourceKind, &str)>> = BTreeMap::new();
        let mut approvals = BTreeSet::new();
        for approval in &self.tenant_merge_approvals.0 {
            ensure!(approvals.insert(&approval.tenant_id), "duplicate approval");
            text(&approval.rationale, 1024)?;
            // These values were validated by their existing typed deserializers;
            // neither the name nor the digest authenticates the operator.
            text(approval.operator_principal_id.as_str(), 128)?;
            text(approval.evidence_digest.as_str(), 64)?;
        }
        let mut counts = Counts {
            mapping_keys: self.mappings.0.len(),
            declared_mapped_rows: 0,
            declared_quarantined_rows: 0,
            mapped_keys: 0,
            quarantined_keys: 0,
            merge_approvals: approvals.len(),
        };
        for item in &self.mappings.0 {
            ensure!(
                item.source_installation_id == self.source_installation_id,
                "foreign installation"
            );
            ensure!(
                keys.insert((
                    item.source_kind,
                    item.old_owner.as_deref(),
                    item.resource_kind
                )),
                "duplicate source key"
            );
            match (item.source_kind, item.old_owner.as_deref()) {
                (SourceKind::Unresolved, None) => ensure!(
                    item.disposition == Disposition::Quarantined,
                    "unresolved ownership"
                ),
                (SourceKind::Unresolved, Some(_)) | (_, None) => anyhow::bail!("owner namespace"),
                (_, Some(owner)) => text(owner, 512)?,
            }
            ensure!(item.source_count <= MAX_NUMBER, "source count range");
            ensure!(
                item.resource_examples.0.len() as u64 <= item.source_count,
                "example count"
            );
            let mut examples = BTreeSet::new();
            for example in &item.resource_examples.0 {
                text(&example.legacy_resource_id, 512)?;
                text(&example.name, 128)?;
                text(example.content_digest.as_str(), 64)?;
                ensure!(
                    examples.insert(&example.legacy_resource_id),
                    "duplicate example"
                );
            }
            text(&item.rationale, 1024)?;
            text(&item.evidence_ref, 256)?;
            text(item.evidence_digest.as_str(), 64)?;
            text(item.source_content_digest.as_str(), 64)?;
            let total = match item.disposition {
                Disposition::Mapped => {
                    let (Some(tenant), Some(_principal), Some(owner)) = (
                        &item.tenant_id,
                        &item.principal_id,
                        item.old_owner.as_deref(),
                    ) else {
                        anyhow::bail!("mapped identity missing");
                    };
                    owners
                        .entry(tenant)
                        .or_default()
                        .insert((item.source_kind, owner));
                    counts.mapped_keys += 1;
                    &mut counts.declared_mapped_rows
                }
                Disposition::Quarantined => {
                    ensure!(
                        item.tenant_id.is_none() && item.principal_id.is_none(),
                        "quarantine cannot grant authority"
                    );
                    counts.quarantined_keys += 1;
                    &mut counts.declared_quarantined_rows
                }
            };
            *total = total
                .checked_add(item.source_count)
                .filter(|n| *n <= MAX_NUMBER)
                .ok_or_else(|| anyhow::anyhow!("declared aggregate range"))?;
        }
        for (tenant, identities) in &owners {
            ensure!(
                identities.len() <= 1 || approvals.contains(tenant),
                "merge approval missing"
            );
        }
        for tenant in approvals {
            ensure!(
                owners
                    .get(tenant)
                    .is_some_and(|identities| identities.len() > 1),
                "approval without merge"
            );
        }
        Ok(counts)
    }
}
