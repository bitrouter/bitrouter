//! Bounded immutable learned evidence. This artifact confers no live authority.
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    identity::{PrincipalId, RevisionId},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Evidence {
    pub profile: Profile,
    pub source_configuration: RevisionId,
    pub source_policy: ResourceRef,
    pub predecessor: Option<ResourceRef>,
    pub creator: PrincipalId,
    pub experiment_digest: String,
    pub input_root: String,
    pub source_fence_digest: String,
    pub effective_head_digest: String,
    pub algorithm: Algorithm,
    pub units: u32,
    pub conclusive: u32,
    pub unavailable: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) enum Profile {
    #[serde(rename = "learned_checkpoint_v1")]
    V1,
}

// The first consumer is native history optimization. Block learner evidence is
// a separate producer/contract and must not be relabelled as native evidence.
#[derive(Clone, Serialize, Deserialize)]
pub(super) enum Algorithm {
    #[serde(rename = "history_optimizer_v1")]
    HistoryV1,
}

fn digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

impl Evidence {
    pub(super) fn validate(&self, reference: &ResourceRef) -> Result<()> {
        ensure!(
            reference.resource_kind == ResourceKind::Checkpoint
                && reference.owner_kind == OwnerKind::Tenant
                && self.source_policy.resource_kind == ResourceKind::Policy
                && self.source_policy != *reference
                && self.predecessor.as_ref().is_none_or(|previous| {
                    previous.resource_kind == ResourceKind::Checkpoint && previous != reference
                }),
            "invalid learned checkpoint identity"
        );
        ensure!(
            [
                self.experiment_digest.as_str(),
                self.input_root.as_str(),
                self.source_fence_digest.as_str(),
                self.effective_head_digest.as_str(),
            ]
            .into_iter()
            .all(digest),
            "invalid learned checkpoint digest"
        );
        ensure!(
            self.units <= 1024
                && self
                    .conclusive
                    .checked_add(self.unavailable)
                    .is_some_and(|count| count <= self.units),
            "learned checkpoint contribution bounds"
        );
        ensure!(
            self.dependencies().iter().all(|dependency| {
                dependency.owner_kind != OwnerKind::Tenant
                    || dependency.owner_id == reference.owner_id
            }),
            "learned checkpoint has foreign dependency"
        );
        Ok(())
    }

    pub(super) fn dependencies(&self) -> Vec<ResourceRef> {
        std::iter::once(self.source_policy.clone())
            .chain(self.predecessor.iter().cloned())
            .collect()
    }
}
