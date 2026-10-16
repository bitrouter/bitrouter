//! Versioned policy blocks and one atomic publication generation per owner.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::allocation::{Arm, BanditConfig, BatchPlan};

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
                    || plan.learner_version != super::allocation::LEARNER_VERSION
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
        let arm = super::allocation::draw_assignment(propensity, seed)?;
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
}
