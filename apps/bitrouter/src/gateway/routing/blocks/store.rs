//! Durable session admission and intent-to-treat policy-block assignments.
//!
//! The owner control record is the database serialization point. Assignment and
//! request intent commit together before a caller may dispatch the selected
//! route. No model request or content evaluation runs inside that transaction.

pub(crate) mod selection;

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set, TransactionTrait};
use serde::{Deserialize, Serialize};

use super::allocation::Arm;
use super::control::{BlockState, BlockStatus, ControlState, EvolutionMode};
use crate::storage::acp_history::is_administrative_update;
use crate::storage::capture::{events, sessions};
use crate::storage::evolution::digest;
use crate::storage::evolution::{EvolutionStore, records};
use crate::storage::execution::{DecisionContext, RouteIntent};
use crate::storage::session_identity::SessionIdentity;

pub(crate) const CONTROL_KIND: &str = "control";
pub(crate) const CONTROL_KEY: &str = "owner";
pub(crate) const SESSION_KIND: &str = "enrollment";
pub(crate) const REQUEST_KIND: &str = "route_intent";

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

#[derive(Clone)]
pub struct BlockPolicyStore {
    pub(crate) store: EvolutionStore,
}

impl BlockPolicyStore {
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
            .filter(crate::storage::legacy::scope("checkpoint_evolution"))
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
            .filter(crate::storage::legacy::scope("acp_canonical_sessions"))
            .col_expr(
                sessions::Column::Head,
                Expr::col(sessions::Column::Head).into(),
            )
            .filter(sessions::Column::SessionKey.eq(&key))
            .exec(&tx)
            .await?;
        let session = sessions::Entity::find_by_id(&key)
            .filter(crate::storage::legacy::scope("acp_canonical_sessions"))
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
            .filter(crate::storage::legacy::scope("checkpoint_evolution"))
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
            .filter(crate::storage::legacy::scope("checkpoint_evolution"))
            .one(&tx)
            .await?;
        let is_new = enrollment_row.is_none();
        let enrollment: SessionEnrollment = if let Some(row) = &enrollment_row {
            serde_json::from_str(&row.body)?
        } else {
            let captured = events::Entity::find()
                .filter(crate::storage::legacy::scope("acp_capture_events"))
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
                let event: crate::storage::capture::CaptureEvent =
                    serde_json::from_str(&node.event_json)?;
                use crate::storage::capture::CaptureKind;
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
        let (enrollment, intent) = selection::select(
            &mut state,
            selection::Input {
                session_key: &key,
                identity,
                request_id,
                context,
                dependencies,
                bypass,
                watermark: session.head,
                enrollment,
                is_new,
            },
            rand::random,
            || chrono::Utc::now().to_rfc3339(),
        )?;
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
