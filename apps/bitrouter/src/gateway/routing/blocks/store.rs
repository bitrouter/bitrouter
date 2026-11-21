//! Read historical policy-block control, enrollment and intent records.

use anyhow::Result;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::{allocation::Arm, control::ControlState};
use crate::storage::{
    evolution::{EvolutionStore, records},
    execution::{DecisionContext, RouteIntent},
    session_identity::SessionIdentity,
};

pub(crate) const CONTROL_KIND: &str = "control";
pub(crate) const CONTROL_KEY: &str = "owner";
pub(crate) const SESSION_KIND: &str = "enrollment";
pub(crate) const REQUEST_KIND: &str = "route_intent";

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
}
