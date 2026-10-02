//! Durable session admission and intent-to-treat policy-block assignments.
//!
//! The owner control record is the database serialization point. Assignment and
//! request intent commit together before a caller may dispatch the selected
//! route. No model request or content evaluation runs inside that transaction.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set, TransactionTrait};
use serde::{Deserialize, Serialize};

use super::bandit::Arm;
use super::control::{BlockDefinition, BlockState, BlockStatus, ControlState, EvolutionMode};
use super::rubric::digest;
use super::store::{EvolutionStore, records};
use crate::acp_trajectory::SessionIdentity;
use crate::acp_trajectory::entities::{events, sessions};

pub(crate) const CONTROL_KIND: &str = "control";
pub(crate) const CONTROL_KEY: &str = "owner";
pub(crate) const SESSION_KIND: &str = "enrollment";
pub(crate) const REQUEST_KIND: &str = "route_intent";

pub mod revisions;

/// ACP setup notifications can precede the first model request. They do not
/// by themselves establish that a turn has already executed. Unknown or
/// malformed updates and all content/tool/usage updates remain conservative.
/// https://agentclientprotocol.com/protocol/v1/schema#sessionupdate
fn is_administrative_update(payload: &serde_json::Value) -> bool {
    use agent_client_protocol::schema::v1::{SessionNotification, SessionUpdate};
    serde_json::from_value::<SessionNotification>(payload.clone()).is_ok_and(|notification| {
        matches!(
            notification.update,
            SessionUpdate::AvailableCommandsUpdate(_)
                | SessionUpdate::ConfigOptionUpdate(_)
                | SessionUpdate::CurrentModeUpdate(_)
                | SessionUpdate::SessionInfoUpdate(_)
        )
    })
}

/// Live route contracts for active and retired experiments use separate keys.
#[derive(Debug, Clone, Default)]
pub struct RoutingDependencies {
    pub current: BTreeMap<String, String>,
    pub archived: BTreeMap<String, String>,
    pub trial_ready: bool,
    pub expected_control_generation: Option<u64>,
}

impl RoutingDependencies {
    fn matches(&self, state: &ControlState, block: &BlockState) -> bool {
        let live = if state
            .blocks
            .get(&block.definition.block_id)
            .is_some_and(|current| current.experiment_id == block.experiment_id)
        {
            self.current.get(&block.definition.block_id)
        } else {
            self.archived.get(&block.experiment_id)
        };
        live == Some(&block.routing_config_digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionContext {
    /// The effective selector at the admission boundary, before this controller
    /// changes it. This never contains a post-hoc task classification or score.
    pub selector: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockAssignment {
    pub assignment_id: String,
    pub experiment_id: String,
    pub block_revision: String,
    pub control_generation: u64,
    pub mode_epoch: u64,
    pub trial_epoch: u64,
    pub batch_sequence: u64,
    pub assignment_sequence: u64,
    pub arm: Arm,
    /// The deployed probability, including all enrollment constraints.
    pub challenger_propensity_ppm: u32,
    pub random_seed: u64,
    /// Concurrent policy revisions are retained even for nonmatching rules.
    pub reference_blocks: BTreeMap<String, String>,
    pub recorded_at: String,
}

/// Observational membership after adoption, not randomized trial evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdoptionMonitoring {
    pub monitoring_id: String,
    pub experiment_id: String,
    pub adoption_revision: String,
    pub assignment_sequence: u64,
    pub control_generation: u64,
    pub mode_epoch: u64,
    pub reference_blocks: BTreeMap<String, String>,
    pub recorded_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEnrollment {
    pub identity: SessionIdentity,
    pub first_watermark: i64,
    pub first_request_id: String,
    pub context: DecisionContext,
    pub assignments: BTreeMap<String, BlockAssignment>,
    /// Some(empty) pins the absence of blocks. None denotes a legacy enrollment
    /// whose assignment/adoption records identify its original experiment.
    #[serde(default)]
    pub block_experiments: Option<BTreeMap<String, String>>,
    /// Policies already adopted when this session first entered routing. An
    /// existing nonparticipant must not silently switch policy mid-session.
    #[serde(default)]
    pub adopted_blocks: BTreeMap<String, String>,
    #[serde(default)]
    pub monitoring: BTreeMap<String, AdoptionMonitoring>,
    /// Preexisting sessions are retained but never opportunistically enrolled
    /// after some of their results have become visible.
    pub admission_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteIntent {
    pub request_id: String,
    pub identity: SessionIdentity,
    pub context: DecisionContext,
    pub selected_route: String,
    pub applied_block: Option<String>,
    pub reason: String,
    pub bypass: Option<String>,
    pub mode_epoch: u64,
    pub control_generation: u64,
    /// Joint assignment, including blocks that did not match this request. The
    /// session's eventual outcome is not copied once per intent.
    pub assignments: BTreeMap<String, String>,
    #[serde(default)]
    pub monitoring: BTreeMap<String, String>,
    pub captured_watermark: i64,
    pub routing_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub archived_routing_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub trial_ready: bool,
    pub recorded_at: String,
}

#[derive(Clone)]
pub struct EvolutionService {
    pub(crate) store: EvolutionStore,
}

impl EvolutionService {
    pub fn new(db: DatabaseConnection, owner: &str) -> Result<Self> {
        Ok(Self {
            store: EvolutionStore::new(db, owner)?,
        })
    }

    /// Reading disabled state does not create an enrollment or control record.
    pub async fn state(&self) -> Result<ControlState> {
        Ok(self
            .store
            .get(CONTROL_KIND, CONTROL_KEY)
            .await?
            .map(|(_, state)| state)
            .unwrap_or_default())
    }

    async fn initialize_control(&self) -> Result<()> {
        self.store
            .initialize(CONTROL_KIND, CONTROL_KEY, None, &ControlState::default())
            .await?;
        Ok(())
    }

    pub async fn set_mode(
        &self,
        mode: EvolutionMode,
        judge_model: Option<String>,
    ) -> Result<ControlState> {
        self.initialize_control().await?;
        let tx = self.store.db.begin().await?;
        let (row, mut state): (_, ControlState) =
            self.store.lock(&tx, CONTROL_KIND, CONTROL_KEY).await?;
        state.set_mode(mode, judge_model)?;
        self.store.save(&tx, row, &state).await?;
        tx.commit().await?;
        Ok(state)
    }

    /// Changing the evaluator must preserve the mode current at the write,
    /// including an Off selected by another local client.
    pub async fn set_judge_model(&self, model: String) -> Result<ControlState> {
        self.initialize_control().await?;
        let tx = self.store.db.begin().await?;
        let (row, mut state): (_, ControlState) =
            self.store.lock(&tx, CONTROL_KIND, CONTROL_KEY).await?;
        state.set_mode(state.mode, Some(model))?;
        self.store.save(&tx, row, &state).await?;
        tx.commit().await?;
        Ok(state)
    }

    /// An operator withdrawal is serialized with admission and publication.
    /// Keep assignments, evaluations, unrelated blocks and mode settings intact.
    pub async fn restore(
        &self,
        request: &super::control::restoration::RestoreRequest,
    ) -> Result<super::control::Publication> {
        self.initialize_control().await?;
        let tx = self.store.db.begin().await?;
        let (row, mut state): (_, ControlState) =
            self.store.lock(&tx, CONTROL_KIND, CONTROL_KEY).await?;
        let previous_generation = state.generation;
        let publication = state.restore(request)?;
        if state.generation != previous_generation {
            self.store.save(&tx, row, &state).await?;
        }
        tx.commit().await?;
        Ok(publication)
    }

    /// The routing adapter supplies a validated, nonsecret dependency digest of
    /// the complete baseline and candidate routes. This primitive never accepts
    /// an unvalidated route directly from a model-produced rubric.
    pub async fn register(
        &self,
        definition: BlockDefinition,
        routing_config_digest: String,
    ) -> Result<ControlState> {
        self.register_checked(definition, routing_config_digest, None, || Ok(()))
            .await
    }

    /// Reviewed UI registration uses a control/mode fence and an idempotent
    /// block identity. A lost response can be retried without enabling a mode,
    /// resetting learned evidence or creating a second experiment.
    pub(crate) async fn register_checked(
        &self,
        definition: BlockDefinition,
        routing_config_digest: String,
        expected_control: Option<(u64, u64)>,
        precondition: impl FnOnce() -> Result<()>,
    ) -> Result<ControlState> {
        ensure!(
            !routing_config_digest.is_empty(),
            "routing dependency digest is required"
        );
        self.initialize_control().await?;
        let tx = self.store.db.begin().await?;
        let (row, mut state): (_, ControlState) =
            self.store.lock(&tx, CONTROL_KIND, CONTROL_KEY).await?;
        if let Some((generation, mode_epoch)) = expected_control {
            if state
                .registration(&definition, &routing_config_digest, None)?
                .is_some()
            {
                tx.commit().await?;
                return Ok(state);
            }
            ensure!(
                state.generation == generation && state.mode_epoch == mode_epoch,
                "evolution settings changed since preview; review the candidate again"
            );
        }
        precondition()?;
        state.register(definition, routing_config_digest)?;
        self.store.save(&tx, row, &state).await?;
        tx.commit().await?;
        Ok(state)
    }

    pub async fn enrollment(
        &self,
        identity: &SessionIdentity,
    ) -> Result<Option<SessionEnrollment>> {
        self.store.ensure_owner(&identity.owner)?;
        Ok(self
            .store
            .get(SESSION_KIND, &identity.key()?)
            .await?
            .map(|(_, value)| value))
    }

    pub async fn intents(&self, identity: &SessionIdentity) -> Result<Vec<RouteIntent>> {
        self.store.ensure_owner(&identity.owner)?;
        let key = identity.key()?;
        records::Entity::find()
            .filter(records::Column::ScopeId.eq(&self.store.scope_id))
            .filter(records::Column::Kind.eq(REQUEST_KIND))
            .filter(records::Column::SessionKey.eq(key))
            .all(&self.store.db)
            .await?
            .into_iter()
            .map(|row| Ok(serde_json::from_str(&row.body)?))
            .collect()
    }

    /// `current_dependencies` is calculated from the serving routing snapshot.
    /// An absent or changed dependency disables that block before dispatch.
    /// Callers establish the authenticated canonical identity before entry.
    pub async fn select(
        &self,
        identity: &SessionIdentity,
        request_id: &str,
        context: DecisionContext,
        current_dependencies: &BTreeMap<String, String>,
    ) -> Result<Option<RouteIntent>> {
        self.select_with_bypass(
            identity,
            request_id,
            context,
            &RoutingDependencies {
                current: current_dependencies.clone(),
                trial_ready: true,
                ..RoutingDependencies::default()
            },
            None,
        )
        .await
    }

    /// An explicit session override or provider continuation retains precedence.
    /// Its request remains in the assigned session, including non-hit intents.
    pub(crate) async fn select_with_bypass(
        &self,
        identity: &SessionIdentity,
        request_id: &str,
        context: DecisionContext,
        dependencies: &RoutingDependencies,
        bypass: Option<&str>,
    ) -> Result<Option<RouteIntent>> {
        let current_dependencies = &dependencies.current;
        self.store.ensure_owner(&identity.owner)?;
        ensure!(
            !request_id.is_empty()
                && !context.selector.is_empty()
                && !context.fingerprint.is_empty(),
            "request identity and context are required"
        );
        if self
            .store
            .get::<ControlState>(CONTROL_KIND, CONTROL_KEY)
            .await?
            .is_none()
        {
            return Ok(None);
        }
        let key = identity.key()?;
        let tx = self.store.db.begin().await?;
        let (control_row, mut state): (_, ControlState) =
            self.store.lock(&tx, CONTROL_KIND, CONTROL_KEY).await?;
        ensure!(
            dependencies
                .expected_control_generation
                .is_none_or(|generation| generation == state.generation),
            "control changed while resolving live routing dependencies; retry"
        );
        sessions::Entity::update_many()
            .col_expr(
                sessions::Column::Head,
                Expr::col(sessions::Column::Head).into(),
            )
            .filter(sessions::Column::SessionKey.eq(&key))
            .exec(&tx)
            .await?;
        let session = sessions::Entity::find_by_id(&key)
            .one(&tx)
            .await?
            .context("native session is not recorded")?;
        ensure!(
            !session.deleted
                && session.owner == identity.owner
                && session.source == identity.source,
            "canonical session is deleted or outside the routing scope"
        );
        // Serialized replay is safe even after a restart. Reusing an ID with
        // another session or request context is an error, never another draw.
        if let Some(row) = records::Entity::find_by_id(self.store.id(REQUEST_KIND, request_id)?)
            .one(&tx)
            .await?
        {
            let intent: RouteIntent = serde_json::from_str(&row.body)?;
            ensure!(
                intent.identity == *identity && intent.context == context,
                "request ID was reused with different routing input"
            );
            ensure!(
                intent.bypass.as_deref() == bypass,
                "request bypass changed on replay"
            );
            ensure!(
                intent.routing_dependencies == *current_dependencies
                    && intent.archived_routing_dependencies == dependencies.archived
                    && intent.trial_ready == dependencies.trial_ready,
                "request intent belongs to obsolete routing dependencies"
            );
            ensure!(
                intent.mode_epoch == state.mode_epoch
                    && intent.control_generation == state.generation,
                "request intent belongs to an obsolete control generation; use a new request ID"
            );
            tx.commit().await?;
            return Ok(Some(intent));
        }
        let enrollment_row = records::Entity::find_by_id(self.store.id(SESSION_KIND, &key)?)
            .one(&tx)
            .await?;
        let is_new = enrollment_row.is_none();
        let mut enrollment: SessionEnrollment = if let Some(row) = &enrollment_row {
            serde_json::from_str(&row.body)?
        } else {
            let captured = events::Entity::find()
                .filter(events::Column::SessionKey.eq(&key))
                .filter(events::Column::Replay.eq(false))
                .all(&tx)
                .await?;
            let mut prompts = 0;
            let mut started = session.history_origin != "new" && session.parent_key.is_none();
            let epoch_start = state
                .trial_started_at
                .as_deref()
                .map(chrono::DateTime::parse_from_rfc3339)
                .transpose()?;
            let mut created_in_epoch = false;
            let mut creation_recorded = false;
            for node in captured {
                let event: bitrouter_sdk::acp::capture::CaptureEvent =
                    serde_json::from_str(&node.event_json)?;
                use bitrouter_sdk::acp::capture::CaptureKind;
                if matches!(event.method.as_str(), "session/new" | "session/fork")
                    && event.kind == CaptureKind::Response
                {
                    let created_at = chrono::DateTime::parse_from_rfc3339(&node.captured_at)?;
                    creation_recorded = true;
                    created_in_epoch |= epoch_start.is_some_and(|start| created_at >= start);
                }
                if event.method == "session/prompt" && event.kind == CaptureKind::Request {
                    prompts += 1;
                }
                if (event.method == "session/prompt" && event.kind == CaptureKind::Response)
                    || (event.method == "session/update"
                        && !is_administrative_update(&event.payload))
                    || event.method.starts_with("terminal/")
                    || event.method.starts_with("fs/")
                {
                    started = true;
                }
            }
            SessionEnrollment {
                identity: identity.clone(),
                first_watermark: session.head,
                first_request_id: request_id.into(),
                context: context.clone(),
                assignments: BTreeMap::new(),
                block_experiments: Some(
                    state
                        .blocks
                        .iter()
                        .map(|(id, block)| (id.clone(), block.experiment_id.clone()))
                        .collect(),
                ),
                monitoring: BTreeMap::new(),
                adopted_blocks: state
                    .blocks
                    .iter()
                    .filter(|(_, block)| {
                        creation_recorded
                            && !started
                            && prompts <= 1
                            && block.status == BlockStatus::Adopted
                    })
                    .map(|(id, block)| (id.clone(), block.experiment_id.clone()))
                    .collect(),
                admission_reason: if started || prompts > 1 || !created_in_epoch {
                    "session_already_started"
                } else {
                    "new_session"
                }
                .into(),
            }
        };
        ensure!(
            enrollment.identity == *identity,
            "session enrollment scope mismatch"
        );
        if is_new && state.mode != EvolutionMode::Off {
            let references = state
                .blocks
                .iter()
                .map(|(id, block)| (id.clone(), block.revision.clone()))
                .collect::<BTreeMap<_, _>>();
            for (id, experiment) in &enrollment.adopted_blocks {
                let block = state
                    .blocks
                    .get(id)
                    .context("adopted block disappeared during admission")?;
                if block.definition.source != identity.source
                    || block.experiment_id != *experiment
                    || !state.dependencies_match(block)
                    || current_dependencies.get(id) != Some(&block.routing_config_digest)
                {
                    continue;
                }
                state.next_assignment_sequence = state
                    .next_assignment_sequence
                    .checked_add(1)
                    .context("monitoring sequence overflow")?;
                enrollment.monitoring.insert(
                    id.clone(),
                    AdoptionMonitoring {
                        monitoring_id: digest(&(experiment, &key, "adoption_monitoring"))?,
                        experiment_id: experiment.clone(),
                        adoption_revision: block.revision.clone(),
                        assignment_sequence: state.next_assignment_sequence,
                        control_generation: state.generation,
                        mode_epoch: state.mode_epoch,
                        reference_blocks: references.clone(),
                        recorded_at: chrono::Utc::now().to_rfc3339(),
                    },
                );
            }
        }
        if is_new
            && enrollment.admission_reason == "new_session"
            && state.mode != EvolutionMode::Off
            && dependencies.trial_ready
        {
            let references: BTreeMap<_, _> = state
                .blocks
                .iter()
                .map(|(id, block)| (id.clone(), block.revision.clone()))
                .collect();
            let eligible: Vec<_> = state
                .blocks
                .iter()
                .filter(|(_, block)| {
                    block.definition.source == identity.source
                        && block.status == BlockStatus::Exploring
                        && !block.batch.closed
                        && state.dependencies_match(block)
                        && current_dependencies.get(&block.definition.block_id)
                            == Some(&block.routing_config_digest)
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in eligible {
                let seed = rand::random();
                let Some(allocation) = state.reserve_trial(&id, &key, seed)? else {
                    continue;
                };
                let block = state
                    .blocks
                    .get(&id)
                    .context("block disappeared during admission")?;
                let assignment = BlockAssignment {
                    assignment_id: digest(&(&block.experiment_id, &key))?,
                    experiment_id: block.experiment_id.clone(),
                    block_revision: block.revision.clone(),
                    control_generation: state.generation,
                    mode_epoch: state.mode_epoch,
                    trial_epoch: state.trial_epoch,
                    batch_sequence: allocation.batch_sequence,
                    assignment_sequence: allocation.assignment_sequence,
                    arm: allocation.arm,
                    challenger_propensity_ppm: allocation.challenger_propensity_ppm,
                    random_seed: seed,
                    reference_blocks: references.clone(),
                    recorded_at: chrono::Utc::now().to_rfc3339(),
                };
                enrollment.assignments.insert(id, assignment);
            }
        }
        let mut intent = RouteIntent {
            request_id: request_id.into(),
            identity: identity.clone(),
            context: context.clone(),
            selected_route: context.selector.clone(),
            applied_block: None,
            reason: "baseline_passthrough".into(),
            bypass: bypass.map(str::to_owned),
            mode_epoch: state.mode_epoch,
            control_generation: state.generation,
            assignments: enrollment
                .assignments
                .iter()
                .map(|(id, a)| (id.clone(), a.assignment_id.clone()))
                .collect(),
            monitoring: enrollment
                .monitoring
                .iter()
                .map(|(id, m)| (id.clone(), m.monitoring_id.clone()))
                .collect(),
            captured_watermark: session.head,
            recorded_at: chrono::Utc::now().to_rfc3339(),
            routing_dependencies: current_dependencies.clone(),
            archived_routing_dependencies: dependencies.archived.clone(),
            trial_ready: dependencies.trial_ready,
        };
        for id in state.blocks.keys() {
            let block = if let Some(experiments) = &enrollment.block_experiments {
                let Some(experiment) = experiments.get(id) else {
                    continue;
                };
                state
                    .experiment(id, Some(experiment))
                    .context("pinned experiment missing")?
            } else if let Some(assignment) = enrollment.assignments.get(id) {
                state
                    .experiment(id, Some(&assignment.experiment_id))
                    .context("assigned experiment missing")?
            } else if let Some(experiment) = enrollment.adopted_blocks.get(id) {
                state
                    .experiment(id, Some(experiment))
                    .context("adopted experiment missing")?
            } else {
                state.original_experiment(id)?
            };
            if block.definition.source != identity.source {
                continue;
            }
            let Some(rule) = block.definition.rules.iter().find(|rule| {
                rule.selector == context.selector
                    && rule
                        .fingerprint
                        .as_ref()
                        .is_none_or(|fingerprint| fingerprint == &context.fingerprint)
            }) else {
                continue;
            };
            if !dependencies.matches(&state, block) || !state.external_dependencies_match(block) {
                intent.reason = "routing_dependency_changed".into();
                continue;
            }
            let (baseline, _) = state.baseline_source(block)?;
            if !dependencies.matches(&state, baseline)
                || !state.external_dependencies_match(baseline)
            {
                intent.reason = "routing_dependency_changed".into();
                continue;
            }
            let baseline_rule = baseline
                .definition
                .rules
                .iter()
                .find(|r| r.selector == rule.selector && r.fingerprint == rule.fingerprint)
                .context("inherited baseline matcher missing")?;
            let assignment = enrollment
                .assignments
                .get(id)
                .filter(|a| a.experiment_id == block.experiment_id);
            // Adoption affects new sessions. A baseline session already in this
            // experiment keeps its arm through later checkpoints; switching it
            // here would contaminate the control arm with challenger execution.
            let candidate = state.baseline_valid(block)
                && ((block.status == BlockStatus::Adopted
                    && (assignment.is_some_and(|a| a.arm == Arm::Challenger)
                        || (assignment.is_none()
                            && enrollment.adopted_blocks.get(id) == Some(&block.experiment_id))))
                    || (block.status == BlockStatus::Exploring
                        && state.mode != EvolutionMode::Off
                        && dependencies.trial_ready
                        && assignment.is_some_and(|a| {
                            a.trial_epoch == state.trial_epoch && a.arm == Arm::Challenger
                        })));
            intent.selected_route = if candidate {
                &rule.challenger_route
            } else {
                &baseline_rule.baseline_route
            }
            .clone();
            intent.applied_block = Some(id.clone());
            intent.reason = if block.status == BlockStatus::Adopted && !candidate {
                if assignment.is_some() {
                    "retained_experiment_baseline"
                } else {
                    "retained_preexisting_baseline"
                }
            } else if block.status == BlockStatus::Adopted {
                "adopted_baseline"
            } else if state.mode == EvolutionMode::Off {
                "evolution_off"
            } else if block.status == BlockStatus::RolledBack {
                "block_rolled_back"
            } else if candidate {
                "sticky_challenger"
            } else {
                "retained_baseline"
            }
            .into();
        }
        if let Some(reason) = bypass {
            intent.selected_route = context.selector;
            intent.applied_block = None;
            intent.reason = reason.into();
        }
        // One transaction makes a process crash between admission and dispatch
        // a recorded, unresolved intent, rather than a missing success-only row.
        if is_new {
            records::Entity::insert(records::ActiveModel {
                record_id: Set(self.store.id(SESSION_KIND, &key)?),
                scope_id: Set(self.store.scope_id.clone()),
                kind: Set(SESSION_KIND.into()),
                session_key: Set(Some(key.clone())),
                revision: Set(0),
                body: Set(serde_json::to_string(&enrollment)?),
            })
            .exec(&tx)
            .await?;
        }
        records::Entity::insert(records::ActiveModel {
            record_id: Set(self.store.id(REQUEST_KIND, request_id)?),
            scope_id: Set(self.store.scope_id.clone()),
            kind: Set(REQUEST_KIND.into()),
            session_key: Set(Some(key)),
            revision: Set(0),
            body: Set(serde_json::to_string(&intent)?),
        })
        .exec(&tx)
        .await?;
        self.store.save(&tx, control_row, &state).await?;
        tx.commit().await?;
        Ok(Some(intent))
    }
}

#[cfg(test)]
mod tests;
