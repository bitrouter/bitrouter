//! Operator mutation and learned-plan publication.

use crate::gateway::routing::blocks::allocation::{BatchPlan, Recommendation};
use crate::gateway::routing::blocks::control::*;
use crate::storage::evolution::digest;
use anyhow::{Result, ensure};
use std::collections::BTreeSet;
pub mod restoration;
pub mod revisions;

impl ControlState {
    pub fn set_mode(&mut self, mode: EvolutionMode, judge_model: Option<String>) -> Result<()> {
        let previous_model = self.judge_model.clone();
        if let Some(model) = judge_model {
            ensure!(!model.trim().is_empty(), "judge model cannot be empty");
            self.judge_model = Some(model);
        }
        ensure!(
            mode != EvolutionMode::Automatic || self.judge_model.is_some(),
            "automatic mode requires a configured judge model"
        );
        if mode != self.mode
            || previous_model != self.judge_model
            || (mode != EvolutionMode::Off && self.feedback_started_at.is_none())
        {
            self.mode_epoch = self
                .mode_epoch
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("mode epoch overflow"))?;
            if (mode == EvolutionMode::Off) != (self.mode == EvolutionMode::Off) {
                self.trial_epoch = self
                    .trial_epoch
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("trial epoch overflow"))?;
                self.trial_started_at =
                    (mode != EvolutionMode::Off).then(|| chrono::Utc::now().to_rfc3339());
            }
            self.mode = mode;
            self.feedback_started_at =
                (mode != EvolutionMode::Off).then(|| chrono::Utc::now().to_rfc3339());
            self.generation = self
                .generation
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("publication generation overflow"))?;
        }
        Ok(())
    }
    pub fn register(&mut self, definition: BlockDefinition, config_digest: String) -> Result<()> {
        definition.validate()?;
        ensure!(
            !self.blocks.contains_key(&definition.block_id),
            "block already exists; use a new experiment revision"
        );
        ensure!(
            definition
                .rules
                .iter()
                .all(|r| r.baseline_route == r.selector),
            "cold start must inherit the current route"
        );
        for other in self.blocks.values() {
            if other.definition.source == definition.source {
                ensure!(
                    !definition.rules.iter().any(|a| other
                        .definition
                        .rules
                        .iter()
                        .any(|b| overlap(a, b))),
                    "policy blocks have overlapping matchers"
                );
            }
        }
        for (id, revision) in &definition.dependencies {
            ensure!(
                self.blocks.get(id).is_some_and(|b| &b.revision == revision),
                "block dependency revision is unavailable"
            );
        }
        let revision = digest(&(&definition, &config_digest))?;
        let experiment_id = digest(&("block-experiment-v1", &revision, self.mode_epoch))?;
        let last_exposure_ppm = definition.bandit.initial_exposure_ppm;
        self.blocks.insert(
            definition.block_id.clone(),
            BlockState {
                definition,
                revision,
                parent_revision: None,
                experiment_id,
                predecessor_experiment_id: None,
                baseline_ancestry: vec![],
                status: BlockStatus::Exploring,
                plan: None,
                routing_config_digest: config_digest,
                batch: EnrollmentBatch::default(),
                assigned_challenger_sessions: 0,
                last_exposure_ppm,
            },
        );
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("publication generation overflow"))?;
        Ok(())
    }
    /// The service validates the effective evidence under database locks before
    /// invoking this transition. Replaying a plan cannot create a new batch or
    /// amplify the exploration probability.
    pub fn apply_plan(
        &mut self,
        block_id: &str,
        plan: BatchPlan,
        cohort_resolved: bool,
    ) -> Result<bool> {
        let experiment = self
            .blocks
            .get(block_id)
            .ok_or_else(|| anyhow::anyhow!("unknown policy block"))?
            .experiment_id
            .clone();
        self.apply_experiment_plan(block_id, &experiment, plan, cohort_resolved)
    }
    pub(crate) fn apply_experiment_plan(
        &mut self,
        block_id: &str,
        experiment: &str,
        plan: BatchPlan,
        cohort_resolved: bool,
    ) -> Result<bool> {
        ensure!(self.mode != EvolutionMode::Off, "evolution is off");
        let archived = self
            .blocks
            .get(block_id)
            .is_none_or(|b| b.experiment_id != experiment);
        let block = self
            .experiment(block_id, Some(experiment))
            .ok_or_else(|| anyhow::anyhow!("unknown experiment"))?;
        ensure!(self.dependencies_match(block), "block dependencies changed");
        ensure!(
            plan.learner_version == crate::gateway::routing::blocks::allocation::LEARNER_VERSION
                && plan.config_digest == block.definition.bandit.digest()?
                && plan.measurement_contract == block.definition.measurement_contract,
            "plan contract does not match the block"
        );
        if block.plan.as_ref().is_some_and(|old| {
            old.evidence_digest == plan.evidence_digest
                && old.learner_version == plan.learner_version
                && old.config_digest == plan.config_digest
                && old.measurement_contract == plan.measurement_contract
                && !(block.status == BlockStatus::Exploring
                    && old.recommendation == Recommendation::Promote
                    && plan.recommendation == Recommendation::Promote
                    && block.batch.closed
                    && cohort_resolved
                    && !archived)
        }) {
            return Ok(false);
        }
        ensure!(
            block.status != BlockStatus::RolledBack,
            "rolled-back experiment cannot resume"
        );
        let block = self
            .experiment_mut(block_id, experiment)
            .ok_or_else(|| anyhow::anyhow!("unknown experiment"))?;
        let previous_revision = block.revision.clone();
        let action = match plan.recommendation {
            Recommendation::Rollback => {
                block.status = BlockStatus::RolledBack;
                "rollback"
            }
            Recommendation::Promote if archived && block.status != BlockStatus::Adopted => {
                "archived_evidence"
            }
            Recommendation::Promote
                if block.status == BlockStatus::Exploring
                    && (!block.batch.closed || !cohort_resolved) =>
            {
                // Persist the validated learner contract so an open cohort can
                // continue after an upgrade. Adoption still waits for its full
                // feedback; equal evidence may enact this plan once ready.
                "allocation"
            }
            Recommendation::Promote => {
                ensure!(
                    block.batch.closed && cohort_resolved,
                    "promotion requires a resolved closed cohort"
                );
                if block.status == BlockStatus::Adopted {
                    "adoption_revalidated"
                } else {
                    block.status = BlockStatus::Adopted;
                    "promote"
                }
            }
            _ if block.status == BlockStatus::Adopted => {
                block.status = BlockStatus::RolledBack;
                "withdraw_unsupported_promotion"
            }
            _ if archived => "archived_evidence",
            _ => {
                if block.batch.closed && cohort_resolved {
                    block.batch = EnrollmentBatch {
                        sequence: block
                            .batch
                            .sequence
                            .checked_add(1)
                            .ok_or_else(|| anyhow::anyhow!("batch sequence overflow"))?,
                        ..EnrollmentBatch::default()
                    };
                    if plan.challenger_propensity_ppm > 0 {
                        block.last_exposure_ppm = plan.challenger_propensity_ppm;
                    }
                }
                "allocation"
            }
        };
        // Only an adopted/rolled-back route changes the dependency revision.
        // An exploration allocation update must not invalidate parallel blocks.
        if !matches!(
            action,
            "allocation" | "adoption_revalidated" | "archived_evidence"
        ) {
            block.parent_revision = Some(previous_revision.clone());
            block.revision = digest(&(&previous_revision, action, &plan.plan_id))?;
        }
        block.plan = Some(plan.clone());
        let revision = block.revision.clone();
        let withdrawn = block.status == BlockStatus::RolledBack;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("publication generation overflow"))?;
        self.publications.push(Publication {
            generation: self.generation,
            block_id: block_id.into(),
            experiment_id: Some(experiment.into()),
            previous_revision,
            revision,
            action: action.into(),
            evidence_digest: Some(plan.evidence_digest),
            operator_reason: None,
            recorded_at: chrono::Utc::now().to_rfc3339(),
        });
        if withdrawn {
            self.withdraw_descendants(experiment)?;
        }
        Ok(true)
    }
    /// Apply an observational quality alarm without treating deployment traffic
    /// as additional randomized evidence for the original promotion.
    pub fn apply_monitoring(
        &mut self,
        block_id: &str,
        monitor: &super::bandit::MonitoringPlan,
    ) -> Result<bool> {
        let experiment = self
            .blocks
            .get(block_id)
            .ok_or_else(|| anyhow::anyhow!("unknown policy block"))?
            .experiment_id
            .clone();
        self.apply_experiment_monitoring(block_id, &experiment, monitor)
    }
    pub(crate) fn apply_experiment_monitoring(
        &mut self,
        block_id: &str,
        experiment: &str,
        monitor: &super::bandit::MonitoringPlan,
    ) -> Result<bool> {
        ensure!(self.mode != EvolutionMode::Off, "evolution is off");
        let block = self
            .experiment(block_id, Some(experiment))
            .ok_or_else(|| anyhow::anyhow!("unknown experiment"))?;
        ensure!(
            block.status == BlockStatus::Adopted && self.dependencies_match(block),
            "adopted block or its dependencies changed"
        );
        ensure!(
            monitor.version == super::bandit::MONITOR_VERSION
                && monitor.config_digest == block.definition.bandit.digest()?
                && monitor.measurement_contract == block.definition.measurement_contract,
            "monitor contract does not match the block"
        );
        if !monitor.rollback {
            return Ok(false);
        }
        let block = self
            .experiment_mut(block_id, experiment)
            .ok_or_else(|| anyhow::anyhow!("unknown experiment"))?;
        let previous_revision = block.revision.clone();
        block.parent_revision = Some(previous_revision.clone());
        block.revision = digest(&(
            &previous_revision,
            "adopted_quality_rollback",
            &monitor.evidence_digest,
        ))?;
        block.status = BlockStatus::RolledBack;
        let revision = block.revision.clone();
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("publication generation overflow"))?;
        self.publications.push(Publication {
            generation: self.generation,
            block_id: block_id.into(),
            experiment_id: Some(experiment.into()),
            previous_revision,
            revision,
            action: "adopted_quality_rollback".into(),
            evidence_digest: Some(monitor.evidence_digest.clone()),
            operator_reason: None,
            recorded_at: chrono::Utc::now().to_rfc3339(),
        });
        self.withdraw_descendants(experiment)?;
        Ok(true)
    }
}
