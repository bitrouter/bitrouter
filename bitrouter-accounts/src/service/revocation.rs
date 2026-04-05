//! Database-backed [`KeyRevocationSet`] implementation.
//!
//! Persists revoked key IDs to the `revoked_keys` table so that
//! revocations survive process restarts. The set is expected to be
//! small (number of API keys, not number of JWTs).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bitrouter_core::auth::revocation::KeyRevocationSet;
use chrono::Utc;
use sea_orm::{DatabaseConnection, EntityTrait, Set};

use crate::entity::revoked_key;

/// Database-backed implementation of [`KeyRevocationSet`].
///
/// Queries the `revoked_keys` table for every `is_revoked` check and
/// inserts a row on `revoke`. Suitable for deployments where revocations
/// must persist across restarts.
pub struct DbRevocationSet {
    db: Arc<DatabaseConnection>,
}

impl DbRevocationSet {
    /// Create a new DB-backed revocation set.
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Self { db }
    }
}

impl KeyRevocationSet for DbRevocationSet {
    fn is_revoked(&self, key_id: &str) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        let key_id = key_id.to_owned();
        let db = self.db.clone();
        Box::pin(async move {
            revoked_key::Entity::find_by_id(&key_id)
                .one(db.as_ref())
                .await
                .unwrap_or(None)
                .is_some()
        })
    }

    fn revoke(&self, key_id: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let key_id = key_id.to_owned();
        let db = self.db.clone();
        Box::pin(async move {
            let now = Utc::now().naive_utc();
            let model = revoked_key::ActiveModel {
                key_id: Set(key_id),
                revoked_at: Set(now),
            };
            // Use insert with on_conflict to handle duplicate revocations gracefully.
            let _ = revoked_key::Entity::insert(model)
                .on_conflict(
                    sea_orm::sea_query::OnConflict::column(revoked_key::Column::KeyId)
                        .do_nothing()
                        .to_owned(),
                )
                .exec(db.as_ref())
                .await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::Database;

    async fn setup_test_db() -> Arc<DatabaseConnection> {
        let db = Database::connect("sqlite::memory:").await.unwrap();

        // Run migrations.
        use sea_orm_migration::MigratorTrait;

        struct TestMigrator;

        impl MigratorTrait for TestMigrator {
            fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
                crate::migration::migrations()
            }
        }

        TestMigrator::up(&db, None).await.unwrap();
        Arc::new(db)
    }

    #[tokio::test]
    async fn db_revocation_set_works() {
        let db = setup_test_db().await;
        let set = DbRevocationSet::new(db);

        assert!(!set.is_revoked("key-1").await);

        set.revoke("key-1").await;
        assert!(set.is_revoked("key-1").await);
        assert!(!set.is_revoked("key-2").await);
    }

    #[tokio::test]
    async fn duplicate_revoke_is_idempotent() {
        let db = setup_test_db().await;
        let set = DbRevocationSet::new(db);

        set.revoke("key-1").await;
        set.revoke("key-1").await;
        assert!(set.is_revoked("key-1").await);
    }
}
