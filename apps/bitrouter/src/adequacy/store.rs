//! Migration reader for sealed pre-v2 adequacy evidence.
//!
//! `adequacy_pins` stores negative safety state. `adequacy_exploration` stores
//! positive legacy state. Production code reads these tables only to compile a
//! v2 migration candidate. New semantic observations use the generic eval
//! exchange; test-only writers remain for deterministic migration fixtures.

use std::collections::BTreeMap;

use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder, Set};

use bitrouter_sdk::{BitrouterError, Result};
use serde::Serialize;

use self::adequacy_exploration::Entity as Exploration;
use self::adequacy_pins::Entity as Pins;
use self::adequacy_reliability_events::Entity as ReliabilityEvents;
use self::adequacy_semantic_success::Entity as SemanticSuccess;
use super::reliability::{ReliabilityEvent, ReliabilityKey, ReliabilityObservation};

/// sea-orm entity for the `adequacy_pins` table.
pub mod adequacy_pins {
    use sea_orm::entity::prelude::*;

    /// One pinned fingerprint — escalated to the policy table's escalation tier
    /// until its cooldown elapses.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "adequacy_pins")]
    pub struct Model {
        /// The request fingerprint that is pinned.
        #[sea_orm(primary_key, auto_increment = false)]
        pub fingerprint: String,
        /// When the pin was last (re)applied, as a Unix timestamp in seconds —
        /// the cooldown clock.
        pub pinned_at_unix: i64,
        /// RFC3339 timestamp of the first time this fingerprint was pinned.
        pub created_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// sea-orm entity for the `adequacy_exploration` table.
pub mod adequacy_exploration {
    use sea_orm::entity::prelude::*;

    /// Learned positive exploration state for one request fingerprint.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "adequacy_exploration")]
    pub struct Model {
        /// The request fingerprint being explored.
        #[sea_orm(primary_key, auto_increment = false)]
        pub fingerprint: String,
        /// Candidate observations seen; drives deterministic trial cadence.
        pub observed: i32,
        /// Consecutive adequate cheap trials.
        pub adequate_trials: i32,
        /// Whether this fingerprint is learned safe and routes to the explore tier.
        pub locked: bool,
        /// RFC3339 timestamp of the last exploration-state update.
        pub updated_at: String,
        /// RFC3339 timestamp of the first time this fingerprint was observed.
        pub created_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod adequacy_semantic_success {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "adequacy_semantic_success")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub evidence_id: String,
        pub fingerprint: String,
        pub task_id: String,
        pub created_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod adequacy_reliability_events {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "adequacy_reliability_events")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub sequence: i64,
        #[sea_orm(unique)]
        pub request_id: String,
        pub route_key: String,
        pub provider: String,
        pub model: String,
        pub credential_class: String,
        pub endpoint_scope: String,
        pub protocol: String,
        pub observation: String,
        pub half_open_probe: bool,
        pub observed_at_unix: i64,
        pub created_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyPin {
    pub fingerprint: String,
    pub pinned_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PersistedExplorationState {
    pub fingerprint: String,
    pub observed: u32,
    pub adequate_trials: u32,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PersistedSemanticSuccess {
    pub evidence_id: String,
    pub fingerprint: String,
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PersistedReliabilityEvent {
    pub sequence: i64,
    pub event: ReliabilityEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReliabilityAppendOutcome {
    Inserted,
    Duplicate,
}

/// sea-orm-backed store over the `adequacy_pins` table.
#[derive(Clone)]
pub struct AdequacyStore {
    db: DatabaseConnection,
}

impl AdequacyStore {
    /// Build a store over a database connection. The database must already carry
    /// the `adequacy_pins` table (`crate::db::run_migrations`).
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Load every pin as `(fingerprint, pinned_at_unix)`. Called once at startup
    /// to warm the in-memory pin cache.
    pub async fn load_all(&self) -> Result<Vec<(String, i64)>> {
        Ok(self
            .load_pins()
            .await?
            .into_iter()
            .map(|row| (row.fingerprint, row.pinned_at_unix))
            .collect())
    }

    /// Load complete pin rows for offline migration and policy compilation.
    pub async fn load_pins(&self) -> Result<Vec<LegacyPin>> {
        let rows = Pins::find()
            .all(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("adequacy load_all: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|row| LegacyPin {
                fingerprint: row.fingerprint,
                pinned_at_unix: row.pinned_at_unix,
            })
            .collect())
    }

    /// Load every positive exploration state row. Called once at startup to
    /// warm trial cadence and cheap-route locks.
    pub async fn load_exploration_all(&self) -> Result<Vec<PersistedExplorationState>> {
        let rows = Exploration::find()
            .all(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("adequacy load_exploration_all: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|row| PersistedExplorationState {
                fingerprint: row.fingerprint,
                observed: row.observed.max(0) as u32,
                adequate_trials: row.adequate_trials.max(0) as u32,
                locked: row.locked,
            })
            .collect())
    }

    pub async fn load_semantic_success_counts(&self) -> Result<BTreeMap<String, u32>> {
        let rows = self.load_semantic_successes().await?;
        let mut counts = BTreeMap::new();
        for row in rows {
            let count = counts.entry(row.fingerprint).or_insert(0_u32);
            *count = count.saturating_add(1);
        }
        Ok(counts)
    }

    /// Load complete semantic-success rows for reproducible compilation.
    pub async fn load_semantic_successes(&self) -> Result<Vec<PersistedSemanticSuccess>> {
        let rows = SemanticSuccess::find().all(&self.db).await.map_err(|e| {
            BitrouterError::internal(format!("adequacy load semantic successes: {e}"))
        })?;
        Ok(rows
            .into_iter()
            .map(|row| PersistedSemanticSuccess {
                evidence_id: row.evidence_id,
                fingerprint: row.fingerprint,
                task_id: row.task_id,
            })
            .collect())
    }

    #[cfg(test)]
    pub async fn record_semantic_success(&self, fingerprint: &str, task_id: &str) -> Result<bool> {
        let evidence_id = format!("{fingerprint}\n{task_id}");
        let row = adequacy_semantic_success::ActiveModel {
            evidence_id: Set(evidence_id),
            fingerprint: Set(fingerprint.to_string()),
            task_id: Set(task_id.to_string()),
            created_at: Set(Utc::now().to_rfc3339()),
        };
        match SemanticSuccess::insert(row)
            .on_conflict(
                OnConflict::column(adequacy_semantic_success::Column::EvidenceId)
                    .do_nothing()
                    .to_owned(),
            )
            .exec(&self.db)
            .await
        {
            Ok(_) => Ok(true),
            Err(DbErr::RecordNotInserted) => Ok(false),
            Err(e) => Err(BitrouterError::internal(format!(
                "adequacy record semantic success: {e}"
            ))),
        }
    }

    #[cfg(test)]
    pub async fn clear_semantic_successes(&self, fingerprint: &str) -> Result<()> {
        SemanticSuccess::delete_many()
            .filter(adequacy_semantic_success::Column::Fingerprint.eq(fingerprint))
            .exec(&self.db)
            .await
            .map_err(|e| {
                BitrouterError::internal(format!("adequacy clear semantic successes: {e}"))
            })?;
        Ok(())
    }

    pub async fn append_reliability_event(
        &self,
        event: &ReliabilityEvent,
    ) -> Result<ReliabilityAppendOutcome> {
        let observed_at_unix = i64::try_from(event.observed_at_unix).map_err(|_| {
            BitrouterError::bad_request("reliability observation timestamp exceeds storage range")
        })?;
        let row = adequacy_reliability_events::ActiveModel {
            sequence: Default::default(),
            request_id: Set(event.request_id.clone()),
            route_key: Set(event.route_key.clone()),
            provider: Set(event.endpoint_key.provider.clone()),
            model: Set(event.endpoint_key.model.clone()),
            credential_class: Set(event.endpoint_key.credential_class.clone()),
            endpoint_scope: Set(event.endpoint_key.endpoint_scope.clone()),
            protocol: Set(event.endpoint_key.protocol.clone()),
            observation: Set(reliability_observation_str(event.observation).to_string()),
            half_open_probe: Set(event.half_open_probe),
            observed_at_unix: Set(observed_at_unix),
            created_at: Set(Utc::now().to_rfc3339()),
        };
        match ReliabilityEvents::insert(row)
            .on_conflict(
                OnConflict::column(adequacy_reliability_events::Column::RequestId)
                    .do_nothing()
                    .to_owned(),
            )
            .exec(&self.db)
            .await
        {
            Ok(_) => Ok(ReliabilityAppendOutcome::Inserted),
            Err(DbErr::RecordNotInserted) => {
                let existing = ReliabilityEvents::find()
                    .filter(adequacy_reliability_events::Column::RequestId.eq(&event.request_id))
                    .one(&self.db)
                    .await
                    .map_err(|error| {
                        BitrouterError::internal(format!(
                            "load duplicate reliability event: {error}"
                        ))
                    })?
                    .ok_or_else(|| {
                        BitrouterError::internal(
                            "duplicate reliability insert did not leave an existing row",
                        )
                    })?;
                let existing = reliability_event_from_row(existing)?;
                if reliability_events_are_semantically_equal(&existing.event, event) {
                    Ok(ReliabilityAppendOutcome::Duplicate)
                } else {
                    Err(BitrouterError::bad_request(format!(
                        "conflicting reliability event for request {}",
                        event.request_id
                    )))
                }
            }
            Err(error) => Err(BitrouterError::internal(format!(
                "append reliability event: {error}"
            ))),
        }
    }

    pub async fn load_reliability_events(&self) -> Result<Vec<PersistedReliabilityEvent>> {
        let rows = ReliabilityEvents::find()
            .order_by_asc(adequacy_reliability_events::Column::Sequence)
            .all(&self.db)
            .await
            .map_err(|error| {
                BitrouterError::internal(format!("load reliability events: {error}"))
            })?;
        rows.into_iter().map(reliability_event_from_row).collect()
    }

    /// Upsert a pin, refreshing the cooldown clock (`pinned_at_unix`) without
    /// resetting `created_at`.
    #[cfg(test)]
    pub async fn upsert_pin(&self, fingerprint: &str, pinned_at_unix: i64) -> Result<()> {
        let row = adequacy_pins::ActiveModel {
            fingerprint: Set(fingerprint.to_string()),
            pinned_at_unix: Set(pinned_at_unix),
            created_at: Set(Utc::now().to_rfc3339()),
        };
        Pins::insert(row)
            .on_conflict(
                OnConflict::column(adequacy_pins::Column::Fingerprint)
                    .update_column(adequacy_pins::Column::PinnedAtUnix)
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("adequacy upsert_pin: {e}")))?;
        Ok(())
    }

    /// Upsert positive exploration state for one fingerprint.
    #[cfg(test)]
    pub async fn upsert_exploration(
        &self,
        fingerprint: &str,
        observed: u32,
        adequate_trials: u32,
        locked: bool,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let row = adequacy_exploration::ActiveModel {
            fingerprint: Set(fingerprint.to_string()),
            observed: Set(observed.min(i32::MAX as u32) as i32),
            adequate_trials: Set(adequate_trials.min(i32::MAX as u32) as i32),
            locked: Set(locked),
            updated_at: Set(now.clone()),
            created_at: Set(now),
        };
        Exploration::insert(row)
            .on_conflict(
                OnConflict::column(adequacy_exploration::Column::Fingerprint)
                    .update_column(adequacy_exploration::Column::Observed)
                    .update_column(adequacy_exploration::Column::AdequateTrials)
                    .update_column(adequacy_exploration::Column::Locked)
                    .update_column(adequacy_exploration::Column::UpdatedAt)
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("adequacy upsert_exploration: {e}")))?;
        Ok(())
    }
}

fn reliability_events_are_semantically_equal(
    left: &ReliabilityEvent,
    right: &ReliabilityEvent,
) -> bool {
    left.request_id == right.request_id
        && left.route_key == right.route_key
        && left.endpoint_key == right.endpoint_key
        && left.observation == right.observation
        && left.half_open_probe == right.half_open_probe
}

fn reliability_observation_str(observation: ReliabilityObservation) -> &'static str {
    match observation {
        ReliabilityObservation::Success => "success",
        ReliabilityObservation::TransientFailure => "transient_failure",
    }
}

fn reliability_event_from_row(
    row: adequacy_reliability_events::Model,
) -> Result<PersistedReliabilityEvent> {
    let observation = match row.observation.as_str() {
        "success" => ReliabilityObservation::Success,
        "transient_failure" => ReliabilityObservation::TransientFailure,
        other => {
            return Err(BitrouterError::internal(format!(
                "unknown persisted reliability observation: {other}"
            )));
        }
    };
    let observed_at_unix = u64::try_from(row.observed_at_unix).map_err(|_| {
        BitrouterError::internal("persisted reliability observation has a negative timestamp")
    })?;
    Ok(PersistedReliabilityEvent {
        sequence: row.sequence,
        event: ReliabilityEvent {
            request_id: row.request_id,
            route_key: row.route_key,
            endpoint_key: ReliabilityKey {
                provider: row.provider,
                model: row.model,
                credential_class: row.credential_class,
                endpoint_scope: row.endpoint_scope,
                protocol: row.protocol,
            },
            observation,
            half_open_probe: row.half_open_probe,
            observed_at_unix,
        },
    })
}
