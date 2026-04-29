//! Database-backed virtual key storage.
//!
//! Virtual keys are short opaque credentials that map 1-to-1 to stored JWTs.
//! The database stores only a SHA-256 hash of the virtual key, not the raw
//! key itself.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::entity::virtual_key;

/// Prefix used for BitRouter virtual keys.
pub const VIRTUAL_KEY_PREFIX: &str = "brv_";

/// Request body used by HTTP clients that create a virtual key from a JWT.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateVirtualKeyRequest {
    pub jwt: String,
}

/// Response body returned when a virtual key is created.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateVirtualKeyResponse {
    pub key: String,
}

/// Return whether a credential has the BitRouter virtual-key shape.
pub fn is_virtual_key(credential: &str) -> bool {
    credential.starts_with(VIRTUAL_KEY_PREFIX)
}

/// Hash a virtual key for durable lookup.
pub fn hash_virtual_key(key: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(key.as_bytes()))
}

/// Virtual key data operations.
pub struct VirtualKeyService<'db> {
    db: &'db DatabaseConnection,
}

impl<'db> VirtualKeyService<'db> {
    pub fn new(db: &'db DatabaseConnection) -> Self {
        Self { db }
    }

    /// Create a fresh virtual key that resolves to the provided JWT.
    ///
    /// The returned raw key is not persisted and should be shown once to the
    /// caller. Persisted lookup uses only the key hash.
    pub async fn create(&self, jwt: &str) -> Result<CreateVirtualKeyResponse, sea_orm::DbErr> {
        let key = generate_virtual_key();
        self.store(&key, jwt).await?;
        Ok(CreateVirtualKeyResponse { key })
    }

    /// Store a caller-provided virtual key for tests or external key managers.
    pub async fn store(&self, key: &str, jwt: &str) -> Result<(), sea_orm::DbErr> {
        let model = virtual_key::ActiveModel {
            key_hash: Set(hash_virtual_key(key)),
            jwt: Set(jwt.to_owned()),
            created_at: Set(Utc::now().naive_utc()),
        };
        model.insert(self.db).await?;
        Ok(())
    }

    /// Resolve a virtual key to its stored JWT.
    pub async fn resolve(&self, key: &str) -> Result<Option<String>, sea_orm::DbErr> {
        let hash = hash_virtual_key(key);
        let row = virtual_key::Entity::find_by_id(hash).one(self.db).await?;
        Ok(row.map(|row| row.jwt))
    }
}

fn generate_virtual_key() -> String {
    let bytes: [u8; 24] = rand::random();
    format!("{VIRTUAL_KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ColumnTrait, Database, EntityTrait, QueryFilter};
    use sea_orm_migration::MigratorTrait;

    async fn setup_test_db() -> Result<DatabaseConnection, Box<dyn std::error::Error>> {
        let db = Database::connect("sqlite::memory:").await?;

        struct TestMigrator;

        impl MigratorTrait for TestMigrator {
            fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
                crate::migration::migrations()
            }
        }

        TestMigrator::up(&db, None).await?;
        Ok(db)
    }

    #[tokio::test]
    async fn create_resolves_without_storing_raw_key() -> Result<(), Box<dyn std::error::Error>> {
        let db = setup_test_db().await?;
        let service = VirtualKeyService::new(&db);
        let jwt = "header.payload.signature";

        let response = service.create(jwt).await?;

        assert!(is_virtual_key(&response.key));
        assert_eq!(service.resolve(&response.key).await?.as_deref(), Some(jwt));

        let stored_raw_key = virtual_key::Entity::find()
            .filter(virtual_key::Column::KeyHash.eq(&response.key))
            .one(&db)
            .await?;
        assert!(stored_raw_key.is_none());

        let stored_hash = virtual_key::Entity::find_by_id(hash_virtual_key(&response.key))
            .one(&db)
            .await?;
        assert!(stored_hash.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn missing_virtual_key_resolves_to_none() -> Result<(), Box<dyn std::error::Error>> {
        let db = setup_test_db().await?;
        let service = VirtualKeyService::new(&db);

        assert!(service.resolve("brv_missing").await?.is_none());
        Ok(())
    }
}
