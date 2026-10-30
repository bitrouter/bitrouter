//! One borrowed H transaction retains the learned closure; it never activates.
use super::*;
use crate::{
    storage::operation_budget::{Error, OperationBudget},
    tenancy::{
        learner::source::{Acquired, Calculated},
        model::Snapshot,
    },
};
use anyhow::Context;
use std::collections::HashMap;

fn changed(reference: &ResourceRef) -> Result<ResourceRef> {
    let mut next = reference.clone();
    next.revision_id = uuid::Uuid::new_v4().to_string().try_into()?;
    Ok(next)
}
fn replace(reference: &mut ResourceRef, changed: &HashMap<ResourceRef, ResourceRef>) {
    if let Some(next) = changed.get(reference) {
        *reference = next.clone();
    }
}
fn dependencies(definition: &Definition) -> Vec<ResourceRef> {
    let mut refs = definition.dependencies();
    if let Definition::Route {
        assessment: Some(profile),
        ..
    } = definition
    {
        refs.push(profile.route.clone());
    }
    refs
}

impl Resources {
    async fn learner_closure(
        &self,
        tx: &sea_orm::DatabaseTransaction,
        roots: &[ResourceRef],
        budget: &OperationBudget<'_>,
        reserve_reload: bool,
    ) -> Result<Vec<Artifact>> {
        let mut queue = roots.to_vec();
        let mut seen = HashSet::new();
        let mut artifacts = Vec::new();
        let mut index = 0;
        while index < queue.len() {
            let reference = queue[index].clone();
            index += 1;
            if !seen.insert(reference.clone()) {
                continue;
            }
            ensure!(seen.len() <= MAX_CLOSURE, Error::Capacity);
            super::super::configuration_store::reserve_learner_resource(
                tx,
                &reference,
                budget,
                if reserve_reload { 6 } else { 3 },
            )
            .await?;
            if reserve_reload {
                budget.charge(1, 0)?;
            }
            let artifact = self.load(tx, &reference).await.map_err(Error::Rejected)?;
            for reference in dependencies(&artifact.definition) {
                if !seen.contains(&reference) && !queue.contains(&reference) {
                    queue.push(reference);
                }
            }
            ensure!(queue.len() <= MAX_CLOSURE, Error::Capacity);
            budget.check()?;
            artifacts.push(artifact);
        }
        Ok(artifacts)
    }

    pub(in crate::tenancy) async fn publish_learner_in(
        &self,
        tx: &sea_orm::DatabaseTransaction,
        state: &Snapshot,
        acquired: &Acquired,
        calculated: &Calculated,
        budget: &OperationBudget<'_>,
    ) -> Result<super::super::configuration_store::StoredConfiguration> {
        let registration = &acquired.registration;
        let input = &registration.input;
        acquired
            .identity
            .check(&acquired.dependencies)
            .map_err(Error::Rejected)?;
        let roots = super::super::configuration::references(&acquired.document);
        let closure = self.learner_closure(tx, &roots, budget, false).await?;
        let old_policy = closure
            .iter()
            .find(|artifact| artifact.reference == registration.profile.policy)
            .context("learner retained source policy absent")?;
        let Definition::Policy {
            name,
            dependencies: old_dependencies,
            ..
        } = &old_policy.definition
        else {
            anyhow::bail!("learner source is not a policy");
        };
        let policy_reference = changed(&old_policy.reference)?;
        let mut checkpoint_reference = changed(&old_policy.reference)?;
        checkpoint_reference.resource_kind = ResourceKind::Checkpoint;
        checkpoint_reference.resource_id = format!(
            "learner_{}",
            &acquired.fence.digest()?.trim_start_matches("sha256:")[..32]
        )
        .try_into()?;
        let units = u32::try_from(acquired.fence.units())?;
        let conclusive = u32::try_from(acquired.fence.conclusive())?;
        let unavailable = units
            .checked_sub(conclusive)
            .context("learned contribution count exceeds units")?;
        let checkpoint = Artifact {
            reference: checkpoint_reference.clone(),
            envelope: None,
            definition: Definition::Checkpoint {
                evidence: checkpoint::Evidence {
                    profile: checkpoint::Profile::V1,
                    source_configuration: input.expected_active_revision.clone(),
                    source_policy: old_policy.reference.clone(),
                    predecessor: old_dependencies
                        .iter()
                        .find(|reference| reference.resource_kind == ResourceKind::Checkpoint)
                        .cloned(),
                    creator: registration.ceiling.principal().clone(),
                    experiment_digest: registration.profile.definition_digest.clone(),
                    input_root: input.evidence_root.clone(),
                    source_fence_digest: acquired.fence.digest()?,
                    effective_head_digest: acquired.fence.effective_heads().to_owned(),
                    algorithm: checkpoint::Algorithm::HistoryV1,
                    units,
                    conclusive,
                    unavailable,
                },
            },
        };
        let mut policy_dependencies = old_dependencies.clone();
        policy_dependencies.push(checkpoint_reference);
        let policy = Artifact {
            reference: policy_reference.clone(),
            envelope: None,
            definition: Definition::Policy {
                document: calculated
                    .step
                    .successor
                    .clone()
                    .unwrap_or_else(|| acquired.policy.clone()),
                name: name.clone(),
                dependencies: policy_dependencies,
            },
        };
        let mut replacements = HashMap::from([(old_policy.reference.clone(), policy_reference)]);
        // Include exposed and hidden routes transitively, even when more than one
        // route shares the policy. Never leave an affected route pointing at R1.
        loop {
            let mut added = false;
            for artifact in &closure {
                if matches!(artifact.definition, Definition::Route { .. })
                    && !replacements.contains_key(&artifact.reference)
                    && dependencies(&artifact.definition)
                        .iter()
                        .any(|reference| replacements.contains_key(reference))
                {
                    replacements.insert(artifact.reference.clone(), changed(&artifact.reference)?);
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
        ensure!(
            replacements
                .keys()
                .all(|reference| reference.owner_kind == OwnerKind::Tenant
                    && reference.owner_id.as_str() == input.tenant.as_str()),
            Error::Rejected(ErrorCode::CapabilityMismatch)
        );
        let mut artifacts = vec![checkpoint, policy];
        for original in &closure {
            let Some(reference) = replacements.get(&original.reference) else {
                continue;
            };
            let Definition::Route { .. } = &original.definition else {
                continue;
            };
            let mut definition = original.definition.clone();
            if let Definition::Route {
                policy,
                core_choice,
                assessment,
                ..
            } = &mut definition
            {
                if let Some(reference) = policy {
                    replace(reference, &replacements);
                }
                if let Some(profile) = core_choice {
                    replace(&mut profile.route, &replacements);
                }
                if let Some(profile) = assessment {
                    replace(&mut profile.route, &replacements);
                }
            }
            let import = manifest::Import {
                reference: reference.clone(),
                definition,
            };
            import.validate()?;
            let selected_policy = import
                .definition
                .policy_reference()
                .map(|reference| {
                    artifacts
                        .iter()
                        .chain(closure.iter())
                        .find(|artifact| &artifact.reference == reference)
                        .context("learned route policy closure absent")
                })
                .transpose()?;
            let envelope = compiler::prepare_route(&import, selected_policy)?;
            artifacts.push(Artifact {
                reference: import.reference,
                definition: import.definition,
                envelope: Some(envelope),
            });
        }
        super::super::configuration_store::reserve_learner_catalog(tx, budget).await?;
        let retained = storage::references(tx).await?;
        ensure!(
            retained
                .len()
                .checked_add(artifacts.len())
                .is_some_and(|count| count <= MAX_CLOSURE),
            Error::Capacity
        );
        for artifact in artifacts {
            manifest::Import {
                reference: artifact.reference.clone(),
                definition: artifact.definition.clone(),
            }
            .validate()?;
            let bytes = manifest::bounded_bytes(&artifact, MAX_ARTIFACT_BYTES)?.len();
            budget.charge(
                4,
                bytes
                    .checked_mul(3)
                    .context("learned resource work overflow")?,
            )?;
            acquired
                .identity
                .check(&acquired.dependencies)
                .map_err(Error::Rejected)?;
            storage::retain(self, tx, &artifact).await?;
            budget.check()?;
        }
        let mut document = acquired.document.clone();
        for reference in &mut document.routes {
            replace(reference, &replacements);
        }
        if let Some(reference) = &mut document.policy {
            replace(reference, &replacements);
        }
        // Reserve the immutable closure's reload before the ordinary compiler
        // allocates it. Its SQL reads remain inside this same RR transaction.
        drop(
            self.learner_closure(
                tx,
                &super::super::configuration::references(&document),
                budget,
                true,
            )
            .await?,
        );
        let retained = super::super::configuration::prepare(
            tx,
            self,
            state,
            &registration.ceiling.caller(),
            &input.tenant,
            &document,
            chrono::Utc::now(),
        )
        .await
        .map_err(Error::Rejected)?;
        acquired
            .identity
            .check(&retained.dependencies)
            .map_err(Error::Rejected)?;
        budget.charge(
            retained.dependencies.len(),
            serde_json::to_vec(&retained.compiled)?.len(),
        )?;
        Ok(retained)
    }
}

#[cfg(all(test, unix))]
impl Resources {
    /// Read-only test witness for the actual authenticated retained artifact.
    pub(crate) async fn native_learned_evidence_for_test(
        &self,
        db: &sea_orm::DatabaseConnection,
        reference: &ResourceRef,
    ) -> Result<serde_json::Value> {
        let artifact = self.load(db, reference).await.map_err(Error::Rejected)?;
        let Definition::Checkpoint { evidence } = artifact.definition else {
            anyhow::bail!("expected actual learned checkpoint");
        };
        evidence.validate(reference)?;
        Ok(serde_json::to_value(evidence)?)
    }
}
