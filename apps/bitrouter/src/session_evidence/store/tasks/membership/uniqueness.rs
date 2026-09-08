//! Recheck competing claims and acceptances across the frozen inspection cuts.

use std::path::{Path, PathBuf};

use super::*;
use crate::session_evidence::native_inputs::{self, Receipt};

struct Group {
    controller: SourceDescriptor,
    spool: PathBuf,
    claims: BTreeMap<(String, String), BTreeSet<String>>,
    occurrences: BTreeMap<(NodeKey, String), BTreeSet<String>>,
    ambiguous: BTreeSet<(NodeKey, String)>,
}

pub(super) struct UniqueInputs {
    groups: BTreeMap<String, Group>,
    pub targets: BTreeSet<String>,
    budget: usize,
}

impl UniqueInputs {
    pub async fn new(
        store: &EvidenceStore,
        db: &impl ConnectionTrait,
        evidence: &AttemptExecutions,
    ) -> Result<Self> {
        let mut groups = BTreeMap::new();
        for input in &evidence.inputs.bindings {
            let id = &input.origin.request.range.source_id;
            if groups.contains_key(id) {
                continue;
            }
            let (controller, registration) = store
                .membership_record(db, &input.controller_registration)
                .await?;
            let spool = PathBuf::from(
                registration.input.raw["payload"]["spool"]
                    .as_str()
                    .context("input group spool missing")?,
            );
            groups.insert(
                id.clone(),
                Group {
                    controller,
                    spool,
                    claims: BTreeMap::new(),
                    occurrences: BTreeMap::new(),
                    ambiguous: BTreeSet::new(),
                },
            );
        }
        Ok(Self {
            groups,
            targets: evidence
                .inputs
                .bindings
                .iter()
                .map(|input| input.native_id.clone())
                .collect(),
            budget: MAX_OBJECT_BYTES,
        })
    }

    pub async fn claims(
        &mut self,
        store: &EvidenceStore,
        db: &impl ConnectionTrait,
        source: &SourceDescriptor,
        range: &SourceRange,
        evidence: &AttemptExecutions,
    ) -> Result<()> {
        if source.format != SourceFormat::Acp
            || source.node.is_some()
            || !self.groups.values().any(|group| {
                group.controller.harness == source.harness
                    && group.controller.locator == source.locator
            })
        {
            return Ok(());
        }
        let (claims, gaps) = store.producer_claims_on(db, range, &self.targets).await?;
        ensure!(
            gaps.is_subset(&evidence.inputs.gaps),
            "input claim inspection gaps omitted"
        );
        for claim in claims {
            let Some(group) = self
                .groups
                .get_mut(&claim.observation.origin.request.range.source_id)
            else {
                continue;
            };
            if group.controller.harness != source.harness
                || group.controller.locator != source.locator
            {
                continue;
            }
            let key = native_inputs::claim_key(&claim.observation.event)
                .context("input claim key missing")?;
            let origins = group.claims.entry(key.clone()).or_default();
            retain_candidate(
                origins,
                &claim.observation.origin.request.record_id,
                key.0.len() + key.1.len(),
                &mut self.budget,
            )?;
        }
        Ok(())
    }

    pub fn receipts(
        &mut self,
        source: &SourceDescriptor,
        receipts: &[Receipt],
        ambiguous: &BTreeSet<(NodeKey, String)>,
    ) -> Result<()> {
        if source.format != SourceFormat::CodexAppServer || source.node.is_some() {
            return Ok(());
        }
        let Some(path) = source.locator.strip_prefix("spool:").map(Path::new) else {
            return Ok(());
        };
        for group in self.groups.values_mut() {
            if source.harness != group.controller.harness
                || source.namespace != group.controller.namespace
                || path.parent() != Some(group.spool.as_path())
            {
                continue;
            }
            for key in ambiguous {
                if !group.ambiguous.contains(key) {
                    ensure!(
                        group.ambiguous.len() < MAX_GRAPH_ITEMS,
                        "input ambiguity detail limit"
                    );
                    let bytes = key.0.native_id.len().saturating_add(key.1.len());
                    ensure!(bytes <= self.budget, "input ambiguity detail limit");
                    self.budget -= bytes;
                    group.ambiguous.insert(key.clone());
                }
            }
            for receipt in receipts {
                let key = (receipt.node.clone(), receipt.native_id.clone());
                let occurrences = group.occurrences.entry(key).or_default();
                retain_candidate(
                    occurrences,
                    &receipt.input.record_id,
                    receipt.node.native_id.len() + receipt.native_id.len(),
                    &mut self.budget,
                )?;
            }
        }
        Ok(())
    }

    pub fn verify(&self, evidence: &AttemptExecutions) -> Result<()> {
        for input in &evidence.inputs.bindings {
            let group = self
                .groups
                .get(&input.origin.request.range.source_id)
                .context("input group missing")?;
            let key = (
                if input.node.harness == Harness::Codex {
                    input.node.native_id.clone()
                } else {
                    String::new()
                },
                input.native_id.clone(),
            );
            ensure!(
                group
                    .claims
                    .get(&key)
                    .is_some_and(|origins| origins.len() == 1
                        && origins.contains(&input.origin.request.record_id)),
                "execution input producer unavailable or ambiguous"
            );
            if input.node.harness == Harness::Codex {
                ensure!(
                    !group
                        .ambiguous
                        .contains(&(input.node.clone(), input.native_id.clone()))
                        && group
                            .occurrences
                            .get(&(input.node.clone(), input.native_id.clone()))
                            .is_some_and(|inputs| inputs.len() == 1
                                && inputs.contains(&input.input.record_id)),
                    "execution input accepted by competing requests"
                );
            }
        }
        Ok(())
    }
}

fn retain_candidate(
    values: &mut BTreeSet<String>,
    value: &str,
    key_bytes: usize,
    budget: &mut usize,
) -> Result<()> {
    // Two distinct candidates are sufficient to disprove uniqueness.
    if values.len() < 2 && !values.contains(value) {
        let bytes = key_bytes.saturating_add(value.len());
        ensure!(bytes <= *budget, "input uniqueness detail limit");
        *budget -= bytes;
        values.insert(value.into());
    }
    Ok(())
}
