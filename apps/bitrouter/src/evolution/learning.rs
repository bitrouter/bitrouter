//! Reconstruct the learner from current canonical assessment and resource heads.
//! Historical checkpoint rows and repeated model calls never add observations.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use bitrouter_sdk::acp::capture::CaptureKind;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};

use super::bandit::{self, BatchPlan, EffectiveObservations, Observation};
use super::control::{BlockStatus, ControlState, EvolutionMode};
use super::evidence::EvidencePacket;
use super::inventory::{INVENTORY_VERSION, coverage_matches};
use super::rubric::{RubricEvaluation, digest};
use super::service::{
    CONTROL_KEY, CONTROL_KIND, EvolutionService, SESSION_KIND, SessionEnrollment,
};
use crate::acp_trajectory::checkpoint::entities::{checkpoints, heads, resources};
use crate::acp_trajectory::checkpoint::types::RESOURCE_MEMBERSHIP_VERSION;
use crate::acp_trajectory::entities::{connections, sessions};
use crate::acp_trajectory::{CanonicalEvent, CanonicalStore};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningReport {
    pub block_id: String,
    pub experiment_id: String,
    pub block_status: BlockStatus,
    pub block_revision: String,
    pub publications: Vec<super::control::Publication>,
    pub archived: bool,
    /// The reviewed experiment's minimum, not a default guessed by the UI.
    /// Older local daemons may omit it; absence means unavailable.
    #[serde(default)]
    pub minimum_families_per_arm: Option<usize>,
    pub plan: BatchPlan,
    pub observations: EffectiveObservations,
    pub monitoring_observations: EffectiveObservations,
    pub monitoring: bandit::MonitoringPlan,
    pub cohort_resolved: bool,
    pub unavailable: BTreeMap<String, Vec<String>>,
    pub published: bool,
}

struct Snapshot {
    control_digest: String,
    session_fences: BTreeMap<String, sessions::Model>,
    ancestor_fences: BTreeMap<String, (bool, Option<String>)>,
    assessment_heads: BTreeMap<String, Option<String>>,
    checkpoint_ids: BTreeSet<String>,
    resource_heads: BTreeMap<String, Option<String>>,
    connection_states: BTreeMap<String, String>,
    coverage_revisions: BTreeMap<String, Option<i64>>,
    report: LearningReport,
}

async fn assessment_head(db: &impl ConnectionTrait, key: &str) -> Result<Option<String>> {
    Ok(heads::Entity::find_by_id(key)
        .one(db)
        .await?
        .and_then(|row| row.revision_id))
}

async fn resource_head(db: &impl ConnectionTrait, checkpoint: &str) -> Result<Option<String>> {
    Ok(resources::Entity::find()
        .filter(resources::Column::CheckpointId.eq(checkpoint))
        .order_by_desc(resources::Column::Revision)
        .one(db)
        .await?
        .map(|row| row.observation_id))
}

/// Union of observed prompt-active intervals. Time outside a prompt is excluded.
/// Missing starts, responses or capture timestamps remain unknown. This is an
/// ACP wall-clock measure, not an estimate of model compute time.
fn prompt_active_ms(events: &[CanonicalEvent]) -> Result<Option<u64>> {
    let mut starts = BTreeMap::new();
    let mut intervals = Vec::new();
    for node in events
        .iter()
        .filter(|node| node.event.method == "session/prompt")
    {
        let Some(call_id) = node.event.call_id else {
            return Ok(None);
        };
        let Some((connection, _)) = node.node_id.rsplit_once(':') else {
            return Ok(None);
        };
        let key = (connection.to_owned(), call_id);
        let time = chrono::DateTime::parse_from_rfc3339(&node.captured_at)?.timestamp_millis();
        match node.event.kind {
            CaptureKind::Request if starts.insert(key.clone(), time).is_some() => return Ok(None),
            CaptureKind::Response => {
                let Some(start) = starts.remove(&key) else {
                    return Ok(None);
                };
                if time < start {
                    return Ok(None);
                }
                intervals.push((start, time));
            }
            _ => {}
        }
    }
    if !starts.is_empty() || intervals.is_empty() {
        return Ok(None);
    }
    intervals.sort_unstable();
    let mut merged: Option<(i64, i64)> = None;
    let mut duration = 0_u64;
    for (start, end) in intervals {
        if let Some((left, right)) = merged {
            if start <= right {
                merged = Some((left, right.max(end)));
            } else {
                duration = duration
                    .checked_add(u64::try_from(right - left)?)
                    .context("prompt duration overflow")?;
                merged = Some((start, end));
            }
        } else {
            merged = Some((start, end));
        }
    }
    if let Some((left, right)) = merged {
        duration = duration
            .checked_add(u64::try_from(right - left)?)
            .context("prompt duration overflow")?;
    }
    Ok(Some(duration))
}

impl EvolutionService {
    /// Read-only inspection. It performs no judge call, enrollment or routing
    /// mutation. Errors caused by concurrent canonical writes require a retry.
    pub async fn learning_status(&self, block_id: &str) -> Result<LearningReport> {
        self.learning_status_experiment(block_id, None).await
    }

    pub async fn learning_status_experiment(
        &self,
        block_id: &str,
        experiment_id: Option<&str>,
    ) -> Result<LearningReport> {
        Ok(self
            .learning_snapshot(block_id, experiment_id)
            .await?
            .report)
    }

    /// Plan and publish against the current canonical evidence in one fenced
    /// transition. A caller does not get to submit arbitrary numeric samples.
    pub async fn reconcile(&self, block_id: &str) -> Result<LearningReport> {
        self.reconcile_checked(block_id, None, || Ok(())).await
    }

    pub(crate) async fn reconcile_checked(
        &self,
        block_id: &str,
        experiment_id: Option<&str>,
        precondition: impl FnOnce() -> Result<()>,
    ) -> Result<LearningReport> {
        let mut snapshot = self.learning_snapshot(block_id, experiment_id).await?;
        let tx = self.store.db.begin().await?;
        let (row, mut state): (_, ControlState) =
            self.store.lock(&tx, CONTROL_KIND, CONTROL_KEY).await?;
        ensure!(
            digest(&state)? == snapshot.control_digest,
            "control changed while reconstructing feedback; retry"
        );
        ensure!(state.mode != EvolutionMode::Off, "evolution is off");
        snapshot.validate(&tx, &self.store).await?;
        precondition()?;
        if state
            .experiment(block_id, Some(&snapshot.report.experiment_id))
            .is_some_and(|block| block.status == BlockStatus::RolledBack)
        {
            // Statistics can still be inspected after withdrawal, but a later
            // favorable posterior draw cannot restart a terminated experiment.
            tx.commit().await?;
            return Ok(snapshot.report);
        }
        snapshot.report.published = if snapshot.report.block_status == BlockStatus::Adopted
            && snapshot.report.monitoring.rollback
        {
            state.apply_experiment_monitoring(
                block_id,
                &snapshot.report.experiment_id,
                &snapshot.report.monitoring,
            )?
        } else {
            state.apply_experiment_plan(
                block_id,
                &snapshot.report.experiment_id,
                snapshot.report.plan.clone(),
                snapshot.report.cohort_resolved,
            )?
        };
        let current = state
            .experiment(block_id, Some(&snapshot.report.experiment_id))
            .context("block disappeared during publication")?;
        snapshot.report.block_status = current.status;
        snapshot.report.block_revision = current.revision.clone();
        snapshot.report.publications = state
            .publications
            .iter()
            .filter(|publication| {
                publication.block_id == block_id
                    && publication.experiment_id.as_deref() == Some(&current.experiment_id)
            })
            .cloned()
            .collect();
        if snapshot.report.published {
            self.store.save(&tx, row, &state).await?;
        }
        tx.commit().await?;
        Ok(snapshot.report)
    }

    async fn learning_snapshot(
        &self,
        block_id: &str,
        experiment_id: Option<&str>,
    ) -> Result<Snapshot> {
        let state = self.state().await?;
        let block = state
            .experiment(block_id, experiment_id)
            .context("unknown experiment")?;
        let canonical = CanonicalStore::new(self.store.db.clone());
        let enrollments = self.store.list::<SessionEnrollment>(SESSION_KIND).await?;
        let mut observations = EffectiveObservations::default();
        let mut monitoring_observations = EffectiveObservations::default();
        let mut unavailable = BTreeMap::new();
        let mut resolved = BTreeSet::new();
        let mut session_fences = BTreeMap::new();
        let mut ancestor_fences = BTreeMap::new();
        let mut assessment_heads = BTreeMap::new();
        let mut checkpoint_ids = BTreeSet::new();
        let mut resource_heads = BTreeMap::new();
        let mut connection_states = BTreeMap::new();
        let mut coverage_revisions = BTreeMap::new();
        for (_, _, enrollment) in enrollments {
            let assignment = enrollment
                .assignments
                .get(block_id)
                .filter(|a| a.experiment_id == block.experiment_id);
            let monitoring = enrollment
                .monitoring
                .get(block_id)
                .filter(|m| m.experiment_id == block.experiment_id);
            ensure!(
                assignment.is_none() || monitoring.is_none(),
                "session has both trial and adoption-monitoring membership"
            );
            let (sequence, arm, is_monitoring) = if let Some(assignment) = assignment {
                (assignment.assignment_sequence, assignment.arm, false)
            } else if let Some(monitoring) = monitoring {
                (
                    monitoring.assignment_sequence,
                    bandit::Arm::Challenger,
                    true,
                )
            } else {
                continue;
            };
            self.store.ensure_owner(&enrollment.identity.owner)?;
            let key = enrollment.identity.key()?;
            let session = sessions::Entity::find_by_id(&key)
                .one(&self.store.db)
                .await?
                .context("assigned session metadata disappeared")?;
            let head = assessment_head(&self.store.db, &key).await?;
            let mut observation = Observation {
                session_key: key.clone(),
                assignment_sequence: sequence,
                family_id: canonical.family_id(&enrollment.identity).await?,
                revision: digest(&(&session, &head))?,
                measurement_contract: block.definition.measurement_contract.clone(),
                arm,
                quality: None,
                total_cost_micro_usd: None,
                latency_ms: None,
                severe_violation: false,
            };
            let mut reasons = Vec::new();
            let mut parent = session.parent_key.clone();
            let mut visited = BTreeSet::from([key.clone()]);
            while let Some(parent_key) = parent {
                ensure!(visited.insert(parent_key.clone()), "cyclic fork lineage");
                let ancestor = sessions::Entity::find_by_id(&parent_key)
                    .one(&self.store.db)
                    .await?
                    .context("fork ancestor metadata disappeared")?;
                ensure!(
                    ancestor.owner == session.owner && ancestor.source == session.source,
                    "fork scope mismatch"
                );
                parent = ancestor.parent_key.clone();
                ancestor_fences.insert(parent_key, (ancestor.deleted, ancestor.parent_key));
            }
            if session.deleted {
                reasons.push("source_deleted".into());
                // Deletion is a resolved absence, never a successful outcome.
                resolved.insert(key.clone());
            } else {
                let view = canonical.effective_assessment(&enrollment.identity).await?;
                ensure!(
                    view.current_watermark == session.head && view.current_revision == head,
                    "assessment changed during learning snapshot; retry"
                );
                reasons.extend(view.reasons.clone());
                if !view.stale && view.current_revision.is_some() {
                    resolved.insert(key.clone());
                }
                connection_states.extend(view.source_capture_states.clone());
                observation.revision = digest(&(
                    &session,
                    &view.current_revision,
                    &view.reasons,
                    &view.source_capture_states,
                    view.resource
                        .as_ref()
                        .map(|resource| &resource.observation_id),
                ))?;
                if let Some(cp) = &view.checkpoint {
                    checkpoint_ids.insert(cp.checkpoint_id.clone());
                    resource_heads.insert(
                        cp.checkpoint_id.clone(),
                        view.resource
                            .as_ref()
                            .map(|resource| resource.observation_id.clone()),
                    );
                    // Only recorded, version-validated rubric payloads enter the
                    // learner. Generic external assessments remain inspectable
                    // but cannot be silently reinterpreted as this rubric.
                    if !view.stale
                        && let Some(revision) = &view.assessment
                        && let Some(assessment) = &revision.input.assessment
                    {
                        let content = canonical
                            .checkpoint_content(&enrollment.identity, &cp.checkpoint_id)
                            .await?;
                        let packet = EvidencePacket::from_checkpoint(&content)?;
                        let evaluation =
                            serde_json::from_str::<RubricEvaluation>(&assessment.explanation);
                        let validated = evaluation.ok().and_then(|evaluation| {
                            let normalized = evaluation
                                .assessment(&packet, &assessment.pipeline_config_digest)
                                .ok()?;
                            if digest(&normalized).ok()? != digest(assessment).ok()? {
                                return None;
                            }
                            Some((evaluation.aggregate(&packet).ok()?, evaluation))
                        });
                        if let Some((quality, evaluation)) = validated {
                            observation.measurement_contract =
                                assessment.pipeline_config_digest.clone();
                            observation.severe_violation = evaluation.severe_violation;
                            if view.reasons.is_empty() {
                                observation.quality = quality.complete_score();
                                observation.latency_ms = prompt_active_ms(&content.events)?;
                            }
                            if observation.quality.is_none() {
                                reasons.push("rubric_or_capture_incomplete".into());
                            }
                        } else {
                            reasons.push("unsupported_or_invalid_rubric_contract".into());
                        }
                    }
                    if !view.stale
                        && let Some(resource) = &view.resource
                    {
                        let mut coverage_current = false;
                        if let Some(coverage) = &resource.gateway_coverage {
                            for (id, expected) in &coverage.revisions {
                                if let Some(previous) =
                                    coverage_revisions.insert(id.clone(), *expected)
                                {
                                    ensure!(
                                        previous == *expected,
                                        "resource observations use different gateway revisions; refresh resources"
                                    );
                                }
                            }
                            coverage_current = coverage.version == INVENTORY_VERSION
                                && coverage.scope == "bitrouter_managed_model_requests"
                                && coverage.reasons.is_empty()
                                && !coverage.revisions.is_empty()
                                && view
                                    .source_capture_states
                                    .keys()
                                    .all(|id| coverage.revisions.contains_key(id))
                                && coverage_matches(
                                    &self.store,
                                    &self.store.db,
                                    &coverage.revisions,
                                )
                                .await?;
                        }
                        let membership_current = resource.membership_version.as_deref()
                            == Some(RESOURCE_MEMBERSHIP_VERSION);
                        if !membership_current {
                            reasons.push("resource_membership_contract_changed".into());
                        }
                        if membership_current
                            && resource.metering_complete
                            && coverage_current
                            && resource.unpriced_requests == 0
                            && resource.unassigned_request_ids.is_empty()
                        {
                            observation.total_cost_micro_usd =
                                u64::try_from(resource.known_cost_micro_usd).ok();
                        } else {
                            reasons.push("session_cost_incomplete".into());
                        }
                    }
                }
                if observation.measurement_contract != block.definition.measurement_contract {
                    reasons.push("measurement_contract_changed".into());
                }
            }
            if !reasons.is_empty() {
                unavailable.insert(key.clone(), reasons);
            }
            assessment_heads.insert(key.clone(), head);
            session_fences.insert(key, session);
            if is_monitoring {
                monitoring_observations.replace(observation)?;
            } else {
                observations.replace(observation)?;
            }
        }
        let plan = bandit::plan(
            &block.definition.bandit,
            &observations,
            &block.definition.measurement_contract,
            Some(block.last_exposure_ppm),
            bandit::trial_seed(&block.experiment_id)?,
        )?;
        // Common random draws keep a resource-only revision from changing a
        // quality alarm through fresh numerical sampling noise.
        let monitoring_seed = digest(&(bandit::MONITOR_VERSION, &block.experiment_id))?;
        let monitoring = bandit::monitor(
            &block.definition.bandit,
            &monitoring_observations,
            &block.definition.measurement_contract,
            u64::from_str_radix(
                monitoring_seed
                    .get(..16)
                    .context("invalid monitoring seed digest")?,
                16,
            )?,
        )?;
        let cohort_resolved = !block.batch.members.is_empty()
            && block.batch.members.keys().all(|key| resolved.contains(key));
        Ok(Snapshot {
            control_digest: digest(&state)?,
            session_fences,
            ancestor_fences,
            assessment_heads,
            checkpoint_ids,
            resource_heads,
            connection_states,
            coverage_revisions,
            report: LearningReport {
                block_id: block_id.into(),
                experiment_id: block.experiment_id.clone(),
                block_status: block.status,
                block_revision: block.revision.clone(),
                publications: state
                    .publications
                    .iter()
                    .filter(|publication| {
                        publication.block_id == block_id
                            && publication.experiment_id.as_deref() == Some(&block.experiment_id)
                    })
                    .cloned()
                    .collect(),
                archived: state
                    .blocks
                    .get(block_id)
                    .is_none_or(|current| current.experiment_id != block.experiment_id),
                minimum_families_per_arm: Some(block.definition.bandit.minimum_families_per_arm),
                plan,
                observations,
                monitoring_observations,
                monitoring,
                cohort_resolved,
                unavailable,
                published: false,
            },
        })
    }
}

impl Snapshot {
    async fn validate(
        &self,
        tx: &DatabaseTransaction,
        store: &super::store::EvolutionStore,
    ) -> Result<()> {
        // Capture takes connection locks before session locks. Use the same
        // order so a concurrent append can never deadlock with publication.
        for (id, expected) in &self.connection_states {
            connections::Entity::update_many()
                .col_expr(
                    connections::Column::Head,
                    Expr::col(connections::Column::Head).into(),
                )
                .filter(connections::Column::ConnectionId.eq(id))
                .exec(tx)
                .await?;
            ensure!(
                connections::Entity::find_by_id(id)
                    .one(tx)
                    .await?
                    .is_some_and(|row| &row.state == expected),
                "capture health changed before publication; retry"
            );
        }
        ensure!(
            self.coverage_revisions
                .keys()
                .all(|id| self.connection_states.contains_key(id)),
            "resource coverage references an unlocked connection"
        );
        ensure!(
            coverage_matches(store, tx, &self.coverage_revisions).await?,
            "gateway inventory changed before publication; refresh resources"
        );
        let keys: BTreeSet<_> = self
            .session_fences
            .keys()
            .chain(self.ancestor_fences.keys())
            .collect();
        for key in keys {
            sessions::Entity::update_many()
                .col_expr(
                    sessions::Column::Head,
                    Expr::col(sessions::Column::Head).into(),
                )
                .filter(sessions::Column::SessionKey.eq(key))
                .exec(tx)
                .await?;
            let row = sessions::Entity::find_by_id(key)
                .one(tx)
                .await?
                .context("session metadata disappeared")?;
            if let Some(expected) = self.session_fences.get(key) {
                ensure!(
                    &row == expected,
                    "canonical prefix changed before publication; retry"
                );
            }
            if let Some(expected) = self.ancestor_fences.get(key) {
                ensure!(
                    (row.deleted, row.parent_key.clone()) == *expected,
                    "inherited source changed before publication; retry"
                );
            }
            if let Some(expected) = self.assessment_heads.get(key) {
                ensure!(
                    assessment_head(tx, key).await? == *expected,
                    "assessment revised before publication; retry"
                );
            }
        }
        for id in &self.checkpoint_ids {
            ensure!(
                checkpoints::Entity::find_by_id(id)
                    .one(tx)
                    .await?
                    .is_some_and(|row| !row.deleted),
                "checkpoint was invalidated before publication; retry"
            );
        }
        for (id, expected) in &self.resource_heads {
            ensure!(
                resource_head(tx, id).await? == *expected,
                "resource evidence changed before publication; retry"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
