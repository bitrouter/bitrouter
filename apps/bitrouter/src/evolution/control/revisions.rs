//! Sequential experiments retain their evidence and baseline provenance.

use super::*;
use anyhow::Context;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedExperiment {
    pub block: BlockState,
    pub superseded_by: String,
    pub retired_at: String,
}

impl ControlState {
    pub fn experiment(&self, block_id: &str, experiment_id: Option<&str>) -> Option<&BlockState> {
        let current = self.blocks.get(block_id)?;
        match experiment_id {
            None => Some(current),
            Some(id) if current.experiment_id == id => Some(current),
            Some(id) => self
                .archived_experiments
                .get(id)
                .map(|archive| &archive.block)
                .filter(|block| block.definition.block_id == block_id),
        }
    }

    pub(super) fn experiment_mut(
        &mut self,
        block_id: &str,
        experiment_id: &str,
    ) -> Option<&mut BlockState> {
        if self
            .blocks
            .get(block_id)
            .is_some_and(|b| b.experiment_id == experiment_id)
        {
            self.blocks.get_mut(block_id)
        } else {
            self.archived_experiments
                .get_mut(experiment_id)
                .map(|archive| &mut archive.block)
                .filter(|block| block.definition.block_id == block_id)
        }
    }

    pub fn original_experiment(&self, block_id: &str) -> Result<&BlockState> {
        let mut block = self.blocks.get(block_id).context("unknown policy block")?;
        let mut seen = BTreeSet::new();
        while let Some(previous) = &block.predecessor_experiment_id {
            ensure!(seen.insert(previous), "cyclic experiment history");
            block = self
                .experiment(block_id, Some(previous))
                .context("experiment predecessor missing")?;
        }
        Ok(block)
    }

    pub fn registration(
        &self,
        definition: &BlockDefinition,
        routing_digest: &str,
        predecessor: Option<&str>,
    ) -> Result<Option<&BlockState>> {
        let definition_digest = digest(definition)?;
        for block in self
            .blocks
            .values()
            .chain(self.archived_experiments.values().map(|a| &a.block))
        {
            if block.definition.block_id == definition.block_id
                && block.predecessor_experiment_id.as_deref() == predecessor
                && block.routing_config_digest == routing_digest
                && digest(&block.definition)? == definition_digest
            {
                return Ok(Some(block));
            }
        }
        Ok(None)
    }

    pub fn baseline_valid(&self, block: &BlockState) -> bool {
        block.baseline_ancestry.iter().all(|id| {
            self.experiment(&block.definition.block_id, Some(id))
                .is_some_and(|ancestor| {
                    ancestor.status == BlockStatus::Adopted
                        && self.external_dependencies_match(ancestor)
                })
        })
    }

    /// Return the last still-supported baseline. A withdrawn ancestor also
    /// withdraws its descendants; its own baseline predates the failed adoption.
    pub fn baseline_source<'a>(
        &'a self,
        block: &'a BlockState,
    ) -> Result<(&'a BlockState, Vec<String>)> {
        let mut ancestry = Vec::new();
        for id in &block.baseline_ancestry {
            let ancestor = self
                .experiment(&block.definition.block_id, Some(id))
                .context("baseline ancestor missing")?;
            if ancestor.status != BlockStatus::Adopted
                || !self.external_dependencies_match(ancestor)
            {
                return Ok((ancestor, ancestry));
            }
            ancestry.push(id.clone());
        }
        Ok((block, ancestry))
    }

    /// Candidate routes may change together; matcher identity stays stable so
    /// a revision never silently drops part of an already adopted policy.
    pub fn revise(
        &mut self,
        definition: BlockDefinition,
        routing_digest: String,
        predecessor: &str,
        reset_to_configured: bool,
    ) -> Result<()> {
        definition.validate()?;
        let previous = self
            .blocks
            .get(&definition.block_id)
            .context("unknown policy block")?
            .clone();
        ensure!(
            previous.experiment_id == predecessor,
            "experiment changed; refresh before revising"
        );
        ensure!(
            definition.source == previous.definition.source,
            "revision cannot change the block's agent source"
        );
        let matchers = |rules: &[BlockRule]| {
            rules
                .iter()
                .map(|r| (r.selector.clone(), r.fingerprint.clone()))
                .collect::<BTreeSet<_>>()
        };
        ensure!(
            matchers(&definition.rules) == matchers(&previous.definition.rules),
            "revision must preserve the block's matchers"
        );
        let (baseline, mut ancestry) = self.baseline_source(&previous)?;
        let inherit_adoption = !reset_to_configured
            && previous.status == BlockStatus::Adopted
            && self.dependencies_match(&previous);
        for rule in &definition.rules {
            let old = baseline
                .definition
                .rules
                .iter()
                .find(|r| r.selector == rule.selector && r.fingerprint == rule.fingerprint)
                .context("baseline matcher missing")?;
            let expected = if reset_to_configured {
                &rule.selector
            } else if inherit_adoption {
                &old.challenger_route
            } else {
                &old.baseline_route
            };
            ensure!(
                &rule.baseline_route == expected,
                "revision must inherit the effective baseline route"
            );
        }
        if reset_to_configured {
            ancestry.clear();
        } else if inherit_adoption {
            ancestry.push(previous.experiment_id.clone());
        }
        for (id, revision) in &definition.dependencies {
            ensure!(
                self.blocks.get(id).is_some_and(|b| &b.revision == revision),
                "block dependency revision is unavailable"
            );
        }
        let experiment_id = digest(&(
            "block-experiment-revision-v1",
            &previous.experiment_id,
            &definition,
            &routing_digest,
            &ancestry,
        ))?;
        ensure!(
            !self.archived_experiments.contains_key(&experiment_id),
            "experiment revision already exists"
        );
        let revision = if reset_to_configured || !self.baseline_valid(&previous) {
            digest(&(&previous.revision, "rebase", &experiment_id))?
        } else {
            // Starting a trial preserves the deployed baseline. Independent
            // blocks must not be invalidated by its new candidate definition.
            previous.revision.clone()
        };
        let block_id = definition.block_id.clone();
        let parent_revision = if revision == previous.revision {
            previous.parent_revision.clone()
        } else {
            Some(previous.revision.clone())
        };
        let next = BlockState {
            last_exposure_ppm: definition.bandit.initial_exposure_ppm,
            definition,
            revision: revision.clone(),
            parent_revision,
            experiment_id: experiment_id.clone(),
            predecessor_experiment_id: Some(previous.experiment_id.clone()),
            baseline_ancestry: ancestry,
            status: BlockStatus::Exploring,
            plan: None,
            routing_config_digest: routing_digest,
            batch: EnrollmentBatch::default(),
            assigned_challenger_sessions: 0,
        };
        self.generation = self
            .generation
            .checked_add(1)
            .context("publication generation overflow")?;
        self.publications.push(Publication {
            generation: self.generation,
            block_id: block_id.clone(),
            experiment_id: Some(experiment_id.clone()),
            previous_revision: previous.revision.clone(),
            revision,
            action: "experiment_revised".into(),
            evidence_digest: None,
            operator_reason: None,
            recorded_at: chrono::Utc::now().to_rfc3339(),
        });
        self.archived_experiments.insert(
            previous.experiment_id.clone(),
            ArchivedExperiment {
                block: previous,
                superseded_by: experiment_id,
                retired_at: chrono::Utc::now().to_rfc3339(),
            },
        );
        self.blocks.insert(block_id, next);
        Ok(())
    }

    pub(super) fn withdraw_descendants(&mut self, withdrawn: &str) -> Result<()> {
        let affected: Vec<_> = self
            .blocks
            .values()
            .chain(self.archived_experiments.values().map(|a| &a.block))
            // A second ancestor withdrawal can change the effective baseline
            // even when this version is already rolled back. Advance its
            // dependency revision for that additional route change as well.
            .filter(|block| block.baseline_ancestry.iter().any(|id| id == withdrawn))
            .map(|block| {
                (
                    block.definition.block_id.clone(),
                    block.experiment_id.clone(),
                )
            })
            .collect();
        for (id, experiment) in affected {
            let block = self
                .experiment_mut(&id, &experiment)
                .context("descendant experiment missing")?;
            let previous_revision = block.revision.clone();
            block.parent_revision = Some(previous_revision.clone());
            block.revision = digest(&(&previous_revision, "baseline_withdrawn", withdrawn))?;
            block.status = BlockStatus::RolledBack;
            let revision = block.revision.clone();
            self.generation = self
                .generation
                .checked_add(1)
                .context("publication generation overflow")?;
            self.publications.push(Publication {
                generation: self.generation,
                block_id: id,
                experiment_id: Some(experiment),
                previous_revision,
                revision,
                action: "inherited_baseline_withdrawn".into(),
                evidence_digest: None,
                operator_reason: None,
                recorded_at: chrono::Utc::now().to_rfc3339(),
            });
        }
        Ok(())
    }
}
