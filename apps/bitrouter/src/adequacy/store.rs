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
use sea_orm::{ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, Set};

use bitrouter_sdk::{BitrouterError, Result};

use self::adequacy_exploration::Entity as Exploration;
use self::adequacy_pins::Entity as Pins;
use self::adequacy_semantic_success::Entity as SemanticSuccess;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedExplorationState {
    pub fingerprint: String,
    pub observed: u32,
    pub adequate_trials: u32,
    pub locked: bool,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adequacy::{AdequacyLedger, Outcome};
    use crate::db;
    use bitrouter_sdk::config::AdequacyConfig;

    async fn store() -> AdequacyStore {
        let db = db::connect("sqlite::memory:").await.unwrap();
        db::run_migrations(&db).await.unwrap();
        AdequacyStore::new(db)
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
