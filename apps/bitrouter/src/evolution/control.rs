//! Versioned policy blocks and one atomic publication generation per owner.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::bandit::{Arm, BanditConfig, BatchPlan, Recommendation};
use super::rubric::digest;

pub mod restoration;
pub mod revisions;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionMode {
    #[default]
    Off,
    Manual,
    Automatic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockRule {
    /// Existing caller selector. A rule never matches arbitrary third-party
    /// traffic outside the configured native ACP source.
    pub selector: String,
    /// Optional deterministic decision-time fingerprint from PolicyTable.
    pub fingerprint: Option<String>,
    /// A complete existing route, including its configured fallback behavior.
    /// Initially this must equal selector, inheriting the working configuration.
    pub baseline_route: String,
    pub challenger_route: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockDefinition {
    pub block_id: String,
    pub source: String,
    pub rationale: String,
    pub rules: Vec<BlockRule>,
    /// Explicit independence assumption for simultaneously explored blocks.
    /// Interacting rules belong in one joint block; disjoint matching alone is
    /// not evidence of statistical independence.
    pub independence_rationale: String,
    pub dependencies: BTreeMap<String, String>,
    pub measurement_contract: String,
    /// Close enrollment at this size and wait for the cohort's feedback before
    /// publishing the next allocation probability. Pending limits may close it
    /// earlier. Existing session assignments survive batch boundaries.
    pub batch_sessions: usize,
    pub bandit: BanditConfig,
}

impl BlockDefinition {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.block_id.trim().is_empty()
                && !self.source.trim().is_empty()
                && !self.rationale.trim().is_empty()
                && !self.independence_rationale.trim().is_empty()
                && !self.measurement_contract.trim().is_empty(),
            "block identity, rationale and measurement contract are required"
        );
        ensure!(!self.rules.is_empty(), "a block needs at least one rule");
        ensure!(
            (1..=10_000).contains(&self.batch_sessions),
            "invalid batch size"
        );
        let mut keys = BTreeSet::new();
        for rule in &self.rules {
            ensure!(
                !rule.selector.trim().is_empty()
                    && !rule.baseline_route.trim().is_empty()
                    && !rule.challenger_route.trim().is_empty(),
                "block rule routes must be nonempty"
            );
            ensure!(
                rule.fingerprint
                    .as_ref()
                    .is_none_or(|f| !f.trim().is_empty()),
                "empty fingerprint"
            );
            ensure!(
                keys.insert((&rule.selector, &rule.fingerprint)),
                "duplicate rule matcher"
            );
        }
        for (index, left) in self.rules.iter().enumerate() {
            ensure!(
                !self
                    .rules
                    .iter()
                    .skip(index + 1)
                    .any(|right| overlap(left, right)),
                "overlapping rules within a block"
            );
        }
        ensure!(
            self.rules
                .iter()
                .any(|r| r.baseline_route != r.challenger_route),
            "challenger must change a route"
        );
        ensure!(
            !self.dependencies.contains_key(&self.block_id),
            "a block cannot depend on itself"
        );
        self.bandit.validate()
    }
}

pub fn overlap(left: &BlockRule, right: &BlockRule) -> bool {
    left.selector == right.selector
        && (left.fingerprint.is_none()
            || right.fingerprint.is_none()
            || left.fingerprint == right.fingerprint)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockStatus {
    Exploring,
    Adopted,
    RolledBack,
    Paused,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnrollmentBatch {
    pub sequence: u64,
    pub members: BTreeMap<String, Arm>,
    pub closed: bool,
}

/// One reservation in a closed-cohort experiment. Native-session eligibility
/// and durable exactly-once enrollment are checked by the admission service.
#[derive(Debug, Clone)]
pub struct TrialAllocation {
    pub assignment_sequence: u64,
    pub batch_sequence: u64,
    pub arm: Arm,
    pub challenger_propensity_ppm: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockState {
    pub definition: BlockDefinition,
    pub revision: String,
    pub parent_revision: Option<String>,
    pub experiment_id: String,
    #[serde(default)]
    pub predecessor_experiment_id: Option<String>,
    /// Adopted experiments supplying this baseline, in root-to-leaf order.
    #[serde(default)]
    pub baseline_ancestry: Vec<String>,
    pub status: BlockStatus,
    pub plan: Option<BatchPlan>,
    /// The config dependency snapshot includes all existing fallback semantics.
    pub routing_config_digest: String,
    pub batch: EnrollmentBatch,
    pub assigned_challenger_sessions: usize,
    /// Holds do not reset the last admitted exploration rate.
    pub last_exposure_ppm: u32,
}

impl BlockState {
    /// Incomplete observations in the last validated plan remain outstanding
    /// across cohorts. Reservations since that plan have no validated feedback.
    fn outstanding_challenger_reservations(&self) -> usize {
        self.plan
            .as_ref()
            .map_or(self.assigned_challenger_sessions, |plan| {
                plan.challenger.incomplete_sessions.saturating_add(
                    self.assigned_challenger_sessions
                        .saturating_sub(plan.challenger.assigned_sessions),
                )
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Publication {
    pub generation: u64,
    pub block_id: String,
    #[serde(default)]
    pub experiment_id: Option<String>,
    pub previous_revision: String,
    pub revision: String,
    pub action: String,
    pub evidence_digest: Option<String>,
    /// Explicit operator feedback is retained separately from measured evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_reason: Option<String>,
    pub recorded_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControlState {
    pub mode: EvolutionMode,
    pub mode_epoch: u64,
    /// Automatic discovery starts with stops observed after this feedback epoch.
    /// It never silently bulk-judges the owner's historical recordings.
    #[serde(default)]
    pub feedback_started_at: Option<String>,
    /// Only off/on transitions revoke a live session's trial. Switching between
    /// manual and automatic feedback preserves its random assignment.
    pub trial_epoch: u64,
    pub trial_started_at: Option<String>,
    pub generation: u64,
    pub judge_model: Option<String>,
    pub blocks: BTreeMap<String, BlockState>,
    #[serde(default)]
    pub archived_experiments: BTreeMap<String, revisions::ArchivedExperiment>,
    pub publications: Vec<Publication>,
    pub next_assignment_sequence: u64,
}

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

    pub fn dependencies_match(&self, block: &BlockState) -> bool {
        self.external_dependencies_match(block) && self.baseline_valid(block)
    }

    pub fn external_dependencies_match(&self, block: &BlockState) -> bool {
        block
            .definition
            .dependencies
            .iter()
            .all(|(id, revision)| self.blocks.get(id).is_some_and(|b| &b.revision == revision))
    }

    /// Shared by durable admission and controlled publication experiments.
    /// Reserve under the owner's control lock and save with session enrollment.
    /// A resolved assessment can still have unknown quality/resources. A zero
    /// allocation holds admission even if that assessment closed the old cohort;
    /// last_exposure_ppm only remembers the rate for a later supported resume.
    pub fn reserve_trial(
        &mut self,
        block_id: &str,
        session_key: &str,
        seed: u64,
    ) -> Result<Option<TrialAllocation>> {
        ensure!(!session_key.is_empty(), "session key is required");
        let block = self
            .blocks
            .get(block_id)
            .ok_or_else(|| anyhow::anyhow!("unknown policy block"))?;
        if self.mode == EvolutionMode::Off
            || block.status != BlockStatus::Exploring
            || block.batch.closed
            || !self.dependencies_match(block)
            || block.plan.as_ref().is_some_and(|plan| {
                plan.challenger_propensity_ppm == 0
                    || plan.learner_version != super::bandit::LEARNER_VERSION
                    || block.definition.bandit.digest().ok().as_ref() != Some(&plan.config_digest)
                    || plan.measurement_contract != block.definition.measurement_contract
            })
        {
            return Ok(None);
        }
        ensure!(
            !block.batch.members.contains_key(session_key),
            "session already reserved in this cohort"
        );
        let block = self
            .blocks
            .get_mut(block_id)
            .ok_or_else(|| anyhow::anyhow!("unknown policy block"))?;
        if block.assigned_challenger_sessions >= block.definition.bandit.maximum_challenger_sessions
            || block.outstanding_challenger_reservations()
                >= block.definition.bandit.maximum_pending_challenger
        {
            block.batch.closed = true;
            return Ok(None);
        }
        let propensity = block.last_exposure_ppm;
        let arm = super::bandit::draw_assignment(propensity, seed)?;
        self.next_assignment_sequence = self
            .next_assignment_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("assignment sequence overflow"))?;
        block.batch.members.insert(session_key.into(), arm);
        block.assigned_challenger_sessions += usize::from(arm == Arm::Challenger);
        let pending_candidates = block
            .batch
            .members
            .values()
            .filter(|arm| **arm == Arm::Challenger)
            .count();
        if block.batch.members.len() >= block.definition.batch_sessions
            || pending_candidates >= block.definition.bandit.maximum_pending_challenger
            || block.outstanding_challenger_reservations()
                >= block.definition.bandit.maximum_pending_challenger
            || block.assigned_challenger_sessions
                >= block.definition.bandit.maximum_challenger_sessions
        {
            block.batch.closed = true;
        }
        Ok(Some(TrialAllocation {
            assignment_sequence: self.next_assignment_sequence,
            batch_sequence: block.batch.sequence,
            arm,
            challenger_propensity_ppm: propensity,
        }))
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
            plan.learner_version == super::bandit::LEARNER_VERSION
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
