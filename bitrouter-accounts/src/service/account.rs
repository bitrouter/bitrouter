use chrono::Utc;
use sea_orm::{prelude::Expr, *};
use uuid::Uuid;

use crate::entity::{account, api_key};
use crate::identity::AccountId;

/// Account and API-key data operations.
pub struct AccountService<'db> {
    db: &'db DatabaseConnection,
}

impl<'db> AccountService<'db> {
    pub fn new(db: &'db DatabaseConnection) -> Self {
        Self { db }
    }

    // ── accounts ──────────────────────────────────────────────

    pub async fn create_account(&self, name: &str) -> Result<account::Model, DbErr> {
        let now = Utc::now().naive_utc();
        let model = account::ActiveModel {
            id: Set(Uuid::new_v4()),
            name: Set(name.to_owned()),
            created_at: Set(now),
            updated_at: Set(now),
        };
        model.insert(self.db).await
    }

    pub async fn get_account(&self, id: AccountId) -> Result<Option<account::Model>, DbErr> {
        account::Entity::find_by_id(id.0).one(self.db).await
    }

    pub async fn list_accounts(&self) -> Result<Vec<account::Model>, DbErr> {
        account::Entity::find().all(self.db).await
    }

    // ── api keys ──────────────────────────────────────────────

    /// Create an API key. Returns the *full plaintext key* (only available at
    /// creation time) together with the persisted model (which stores only the
    /// hash).
    pub async fn create_api_key(
        &self,
        account_id: AccountId,
        name: &str,
        plaintext_key: &str,
        key_hash: &str,
    ) -> Result<api_key::Model, DbErr> {
        let now = Utc::now().naive_utc();
        let prefix = &plaintext_key[..std::cmp::min(8, plaintext_key.len())];
        let model = api_key::ActiveModel {
            id: Set(Uuid::new_v4()),
            account_id: Set(account_id.0),
            name: Set(name.to_owned()),
            prefix: Set(format!("{prefix}...")),
            key_hash: Set(key_hash.to_owned()),
            created_at: Set(now),
            expires_at: Set(None),
            revoked_at: Set(None),
        };
        model.insert(self.db).await
    }

    /// Look up an API key by its hash. Returns `None` if the key doesn't
    /// exist or has been revoked / expired.
    pub async fn resolve_api_key(
        &self,
        key_hash: &str,
    ) -> Result<Option<(AccountId, api_key::Model)>, DbErr> {
        let row = api_key::Entity::find()
            .filter(api_key::Column::KeyHash.eq(key_hash))
            .filter(api_key::Column::RevokedAt.is_null())
            .one(self.db)
            .await?;

        match row {
            Some(k) => {
                // Check expiry.
                if let Some(exp) = k.expires_at
                    && Utc::now().naive_utc() > exp {
                        return Ok(None);
                    }
                let aid = AccountId(k.account_id);
                Ok(Some((aid, k)))
            }
            None => Ok(None),
        }
    }

    pub async fn revoke_api_key(&self, key_id: Uuid) -> Result<(), DbErr> {
        let now = Utc::now().naive_utc();
        api_key::Entity::update_many()
            .filter(api_key::Column::Id.eq(key_id))
            .col_expr(api_key::Column::RevokedAt, Expr::value(now))
            .exec(self.db)
            .await?;
        Ok(())
    }

    pub async fn list_api_keys(&self, account_id: AccountId) -> Result<Vec<api_key::Model>, DbErr> {
        api_key::Entity::find()
            .filter(api_key::Column::AccountId.eq(account_id.0))
            .filter(api_key::Column::RevokedAt.is_null())
            .all(self.db)
            .await
    }
}
