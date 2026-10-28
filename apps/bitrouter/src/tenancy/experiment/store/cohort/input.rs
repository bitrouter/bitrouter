//! Immutable acquisition output, never authority to read or publish later.
use super::*;
use crate::{
    harness::trajectory::owned::entities::records::Model, storage::evaluation::canonical_digest,
};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct Sources(BTreeMap<(String, String), String>);

impl Sources {
    pub(super) fn external(
        &mut self,
        kind: &str,
        id: &str,
        digest: String,
        budget: &OperationBudget<'_>,
    ) -> Result<()> {
        let key = (kind.to_owned(), id.to_owned());
        if let Some(previous) = self.0.get(&key) {
            ensure!(previous == &digest, "source changed during acquisition");
        } else {
            budget.charge(0, kind.len() + id.len() + digest.len())?;
            self.0.insert(key, digest);
        }
        Ok(())
    }

    pub(super) fn record(&mut self, row: &Model, budget: &OperationBudget<'_>) -> Result<()> {
        // Include mutable kind/revision and immutable physical/creator edges.
        // Step bodies include the exact original receipt, publication and ACK.
        let digest = canonical_digest(&(
            "owned-native-source-row-v1",
            &row.installation_id,
            &row.tenant_id,
            &row.creator_principal_id,
            &row.scope_id,
            &row.session_key,
            row.revision,
            &row.body,
        ))?;
        let key = (row.kind.clone(), row.record_id.clone());
        if let Some(previous) = self.0.get(&key) {
            ensure!(
                previous == &digest,
                "source changed within owned acquisition"
            );
        } else {
            budget.charge(0, key.0.len() + key.1.len() + digest.len())?;
            self.0.insert(key, digest);
        }
        Ok(())
    }
}

/// Only the complete validated reader can produce this value. The frozen Eval
/// root alone is insufficient: mutable counter/unit/step/ACK edges are separate.
pub(in crate::tenancy) struct ValidatedInput {
    snapshot: EvalEvidenceSnapshot,
    fence: SourceFence,
}

/// Data used to detect stale computation, not a serialized authorization token.
/// Future assessment producers must append their current head/source fences;
/// operational Eval entries cannot imply conclusive Human or paid judgement.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(in crate::tenancy) struct SourceFence {
    version: u32,
    namespace: String,
    profile: String,
    snapshot: String,
    units: usize,
    conclusive: usize,
    effective_heads: String,
    sources: Vec<(String, String, String)>,
}

impl ValidatedInput {
    pub(super) fn new(
        scope: &Scope,
        profile: &Profile,
        snapshot: EvalEvidenceSnapshot,
        sources: Sources,
        units: usize,
        conclusive: usize,
        budget: &OperationBudget<'_>,
    ) -> Result<Self> {
        ensure!(
            units <= MAX_UNITS as usize && conclusive <= units,
            Error::Capacity
        );
        let effective_heads = canonical_digest(
            &sources
                .0
                .iter()
                .filter(|((kind, _), _)| kind == "native_assessment_head_v1")
                .collect::<Vec<_>>(),
        )?;
        let fence = SourceFence {
            version: 2,
            namespace: scope.physical_key(KeyDomain::NativeExperimentPending, "", &[])?,
            profile: canonical_digest(profile)?,
            snapshot: canonical_digest(&snapshot)?,
            units,
            conclusive,
            effective_heads,
            sources: sources
                .0
                .into_iter()
                .map(|((kind, id), digest)| (kind, id, digest))
                .collect(),
        };
        budget.check()?;
        Ok(Self { snapshot, fence })
    }

    /// Plain data leaving acquisition; retaining it confers no current grant.
    pub(in crate::tenancy) fn into_parts(self) -> (EvalEvidenceSnapshot, SourceFence) {
        (self.snapshot, self.fence)
    }
}

impl SourceFence {
    pub(in crate::tenancy) fn digest(&self) -> Result<String> {
        canonical_digest(self)
    }
    pub(in crate::tenancy) fn effective_heads(&self) -> &str {
        &self.effective_heads
    }
    pub(in crate::tenancy) fn conclusive(&self) -> usize {
        self.conclusive
    }
    pub(in crate::tenancy) fn units(&self) -> usize {
        self.units
    }

    pub(super) fn check_selection(
        &self,
        binding: &Binding,
        profile: &Profile,
        snapshot: &EvalEvidenceSnapshot,
    ) -> Result<()> {
        let (scope, _) = Scope::for_session(binding.authority());
        ensure!(
            self.namespace == scope.physical_key(KeyDomain::NativeExperimentPending, "", &[])?
                && self.profile == canonical_digest(profile)?
                && self.snapshot == canonical_digest(snapshot)?,
            "owned learning selection changed"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
