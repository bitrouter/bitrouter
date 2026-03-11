use chrono::Utc;
use sea_orm::*;
use uuid::Uuid;

use crate::entity::{account, rotated_pubkey};
use crate::identity::AccountId;

/// Account and public-key data operations.
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
            master_pubkey: Set(None),
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

    // ── public key authentication ─────────────────────────────

    /// Find an account by its current master public key.
    pub async fn find_by_pubkey(&self, pubkey: &str) -> Result<Option<account::Model>, DbErr> {
        account::Entity::find()
            .filter(account::Column::MasterPubkey.eq(pubkey))
            .one(self.db)
            .await
    }

    /// Create a new account with the given public key.
    /// The account name is auto-generated from the pubkey prefix.
    pub async fn create_with_pubkey(&self, pubkey: &str) -> Result<account::Model, DbErr> {
        let now = Utc::now().naive_utc();
        let name = format!("account-{}", &pubkey[..16.min(pubkey.len())]);
        let model = account::ActiveModel {
            id: Set(Uuid::new_v4()),
            name: Set(name),
            master_pubkey: Set(Some(pubkey.to_owned())),
            created_at: Set(now),
            updated_at: Set(now),
        };
        model.insert(self.db).await
    }

    /// Find an existing account by pubkey, or create one.
    ///
    /// Before auto-creating, checks the rotated_pubkeys table — if the pubkey
    /// was previously rotated away from an account, returns `None` to signal
    /// that the caller should reject the request (the key is stale).
    pub async fn find_or_create_by_pubkey(
        &self,
        pubkey: &str,
    ) -> Result<Option<account::Model>, DbErr> {
        // Check current active pubkey.
        if let Some(account) = self.find_by_pubkey(pubkey).await? {
            return Ok(Some(account));
        }

        // Check if this pubkey was rotated away — reject if so.
        let rotated = rotated_pubkey::Entity::find()
            .filter(rotated_pubkey::Column::Pubkey.eq(pubkey))
            .one(self.db)
            .await?;
        if rotated.is_some() {
            return Ok(None);
        }

        // New pubkey — auto-create account.
        let account = self.create_with_pubkey(pubkey).await?;
        Ok(Some(account))
    }

    /// Rotate the master public key for an account.
    ///
    /// The old pubkey is stored in `rotated_pubkeys` to prevent orphan
    /// account creation from stale JWTs.
    pub async fn rotate_pubkey(
        &self,
        account_id: AccountId,
        new_pubkey: &str,
    ) -> Result<Option<account::Model>, DbErr> {
        let account = account::Entity::find_by_id(account_id.0)
            .one(self.db)
            .await?;

        let Some(account) = account else {
            return Ok(None);
        };

        // Store the old pubkey in rotation history.
        if let Some(old_pubkey) = &account.master_pubkey {
            let now = Utc::now().naive_utc();
            let rotated = rotated_pubkey::ActiveModel {
                id: Set(Uuid::new_v4()),
                account_id: Set(account_id.0),
                pubkey: Set(old_pubkey.clone()),
                rotated_at: Set(now),
            };
            rotated.insert(self.db).await?;
        }

        // Update the account with the new pubkey.
        let now = Utc::now().naive_utc();
        let mut active: account::ActiveModel = account.into();
        active.master_pubkey = Set(Some(new_pubkey.to_owned()));
        active.updated_at = Set(now);
        let updated = active.update(self.db).await?;

        Ok(Some(updated))
    }
}
