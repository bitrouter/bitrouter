//! Persistence for the adequacy ledger's escalation pins and exploration state.
//!
//! `adequacy_pins` stores negative safety state. `adequacy_exploration` stores
//! positive learning state: trial cadence and learned cheap-route locks. The
//! ledger is the single writer; the only reader is the ledger's startup warm-up.
//! Every query goes through sea-orm, so the store works unchanged on whichever
//! backend `database.url` selects (SQLite / Postgres / MySQL), mirroring
//! [`crate::metering::MeteringStore`].

use std::collections::BTreeMap;

use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder, Set};

use bitrouter_sdk::{BitrouterError, Result};

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
        pub observed_at_unix: i64,
        pub created_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedExplorationState {
    pub fingerprint: String,
    pub observed: u32,
    pub adequate_trials: u32,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
        let rows = Pins::find()
            .all(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("adequacy load_all: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|row| (row.fingerprint, row.pinned_at_unix))
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
        let rows = SemanticSuccess::find().all(&self.db).await.map_err(|e| {
            BitrouterError::internal(format!("adequacy load semantic successes: {e}"))
        })?;
        let mut counts = BTreeMap::new();
        for row in rows {
            let count = counts.entry(row.fingerprint).or_insert(0_u32);
            *count = count.saturating_add(1);
        }
        Ok(counts)
    }

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
                if existing.event == *event {
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
            observed_at_unix,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adequacy::reliability::{ReliabilityEvent, ReliabilityKey, ReliabilityObservation};
    use crate::adequacy::{AdequacyLedger, Outcome};
    use crate::db;
    use bitrouter_sdk::config::AdequacyConfig;

    async fn store() -> AdequacyStore {
        let db = db::connect("sqlite::memory:").await.unwrap();
        db::run_migrations(&db).await.unwrap();
        AdequacyStore::new(db)
    }

    fn reliability_event(
        request_id: &str,
        observation: ReliabilityObservation,
        observed_at_unix: u64,
    ) -> ReliabilityEvent {
        ReliabilityEvent {
            request_id: request_id.to_string(),
            route_key: "bitrouter:canary-weak".to_string(),
            endpoint_key: ReliabilityKey {
                provider: "bitrouter".to_string(),
                model: "canary-weak".to_string(),
                credential_class: "default:x_api_key".to_string(),
                endpoint_scope: "127.0.0.1:18090".to_string(),
                protocol: "chat_completions".to_string(),
            },
            observation,
            observed_at_unix,
        }
    }

    #[tokio::test]
    async fn reliability_events_round_trip_in_database_order_and_are_idempotent() {
        let store = store().await;
        let first = reliability_event("request-1", ReliabilityObservation::TransientFailure, 100);
        let second = reliability_event("request-2", ReliabilityObservation::Success, 101);

        assert_eq!(
            store.append_reliability_event(&first).await.unwrap(),
            ReliabilityAppendOutcome::Inserted,
        );
        assert_eq!(
            store.append_reliability_event(&second).await.unwrap(),
            ReliabilityAppendOutcome::Inserted,
        );
        assert_eq!(
            store.append_reliability_event(&first).await.unwrap(),
            ReliabilityAppendOutcome::Duplicate,
        );

        let rows = store.load_reliability_events().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].sequence < rows[1].sequence);
        assert_eq!(rows[0].event, first);
        assert_eq!(rows[1].event, second);
    }

    #[tokio::test]
    async fn reliability_conflicting_duplicate_is_rejected() {
        let store = store().await;
        let first = reliability_event("request-1", ReliabilityObservation::TransientFailure, 100);
        store.append_reliability_event(&first).await.unwrap();
        let conflicting = reliability_event("request-1", ReliabilityObservation::Success, 100);

        let error = store
            .append_reliability_event(&conflicting)
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("conflicting reliability event for request request-1")
        );
    }

    #[tokio::test]
    async fn upsert_then_load_round_trips() {
        let store = store().await;
        store.upsert_pin("after_edit", 1000).await.unwrap();
        store.upsert_pin("after_run", 2000).await.unwrap();
        let mut rows = store.load_all().await.unwrap();
        rows.sort();
        assert_eq!(
            rows,
            vec![
                ("after_edit".to_string(), 1000),
                ("after_run".to_string(), 2000),
            ]
        );
    }

    #[tokio::test]
    async fn upsert_refreshes_the_cooldown_clock_without_duplicating() {
        let store = store().await;
        store.upsert_pin("after_edit", 1000).await.unwrap();
        store.upsert_pin("after_edit", 5000).await.unwrap();
        assert_eq!(
            store.load_all().await.unwrap(),
            vec![("after_edit".to_string(), 5000)]
        );
    }

    #[tokio::test]
    async fn a_pin_survives_a_restart_via_persistence() {
        let db = db::connect("sqlite::memory:").await.unwrap();
        db::run_migrations(&db).await.unwrap();
        let cfg = AdequacyConfig {
            enabled: true,
            escalation_tier: None,
            escalation_threshold: 1,
            pin_cooldown_secs: 0,
            ..Default::default()
        };
        // First ledger: a failure pins the fingerprint and persists it.
        let ledger = AdequacyLedger::load(&cfg, AdequacyStore::new(db.clone())).await;
        ledger
            .observe(
                "after_edit",
                Outcome::StaticDowngrade {
                    cause: crate::adequacy::InadequacyCause::ProviderPermanent,
                },
            )
            .await;
        assert!(ledger.is_pinned("after_edit"));
        // A fresh ledger over the same db warms its cache from the stored pin.
        let reloaded = AdequacyLedger::load(&cfg, AdequacyStore::new(db.clone())).await;
        assert!(
            reloaded.is_pinned("after_edit"),
            "the pin must survive via persistence"
        );
    }

    #[tokio::test]
    async fn an_exploration_lock_survives_a_restart_via_persistence() {
        let db = db::connect("sqlite::memory:").await.unwrap();
        db::run_migrations(&db).await.unwrap();
        let cfg = AdequacyConfig {
            enabled: true,
            escalation_tier: None,
            escalation_threshold: 1,
            pin_cooldown_secs: 0,
            explore_enabled: true,
            explore_tier: Some("cheap".to_string()),
            explore_interval: 1,
            explore_threshold: 2,
            ..Default::default()
        };

        let ledger = AdequacyLedger::load(&cfg, AdequacyStore::new(db.clone())).await;
        ledger
            .observe(
                "tool_followup",
                Outcome::Exploration {
                    trialed: true,
                    cause: crate::adequacy::InadequacyCause::None,
                },
            )
            .await;
        ledger
            .observe(
                "tool_followup",
                Outcome::Exploration {
                    trialed: true,
                    cause: crate::adequacy::InadequacyCause::None,
                },
            )
            .await;
        assert!(ledger.is_locked("tool_followup"));

        let reloaded = AdequacyLedger::load(&cfg, AdequacyStore::new(db.clone())).await;
        assert!(
            reloaded.is_locked("tool_followup"),
            "learned cheap-route locks should survive daemon restart"
        );
    }

    #[tokio::test]
    async fn exploration_cadence_survives_a_restart_via_persistence() {
        let db = db::connect("sqlite::memory:").await.unwrap();
        db::run_migrations(&db).await.unwrap();
        let cfg = AdequacyConfig {
            enabled: true,
            escalation_tier: None,
            escalation_threshold: 1,
            pin_cooldown_secs: 0,
            explore_enabled: true,
            explore_tier: Some("cheap".to_string()),
            explore_interval: 2,
            explore_threshold: 3,
            ..Default::default()
        };

        let ledger = AdequacyLedger::load(&cfg, AdequacyStore::new(db.clone())).await;
        ledger
            .observe(
                "tool_followup",
                Outcome::Exploration {
                    trialed: false,
                    cause: crate::adequacy::InadequacyCause::None,
                },
            )
            .await;
        ledger
            .observe(
                "tool_followup",
                Outcome::Exploration {
                    trialed: false,
                    cause: crate::adequacy::InadequacyCause::None,
                },
            )
            .await;
        assert!(ledger.should_trial("tool_followup"));

        let reloaded = AdequacyLedger::load(&cfg, AdequacyStore::new(db.clone())).await;
        assert!(
            reloaded.should_trial("tool_followup"),
            "trial cadence should survive daemon restart"
        );
    }

    #[tokio::test]
    async fn request_lock_waits_for_distinct_task_successes_when_configured() {
        let db = db::connect("sqlite::memory:").await.unwrap();
        db::run_migrations(&db).await.unwrap();
        let cfg = AdequacyConfig {
            enabled: true,
            explore_enabled: true,
            explore_tier: Some("cheap".to_string()),
            explore_interval: 1,
            explore_threshold: 1,
            min_semantic_successes_for_lock: 2,
            ..Default::default()
        };
        let request_key = "codex|responses|tool_followup|-|-|exec_command";
        let store = AdequacyStore::new(db.clone());
        let ledger = AdequacyLedger::load(&cfg, store.clone()).await;

        ledger
            .observe(
                request_key,
                Outcome::Exploration {
                    trialed: true,
                    cause: crate::adequacy::InadequacyCause::None,
                },
            )
            .await;
        assert!(ledger.is_request_qualified(request_key));
        assert!(!ledger.is_locked(request_key));

        store
            .record_semantic_success(request_key, "terminal-bench/regex-log")
            .await
            .unwrap();
        let one_success = AdequacyLedger::load(&cfg, store.clone()).await;
        assert_eq!(one_success.semantic_successes(request_key), 1);
        assert!(!one_success.is_locked(request_key));

        store
            .record_semantic_success(request_key, "terminal-bench/fix-git")
            .await
            .unwrap();
        let two_successes = AdequacyLedger::load(&cfg, store).await;
        assert_eq!(two_successes.semantic_successes(request_key), 2);
        assert!(two_successes.is_locked(request_key));
    }
}
