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
            caip10_identity: Set(None),
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
            .filter(account::Column::Caip10Identity.eq(pubkey))
            .one(self.db)
            .await
    }

    /// Create a new account with the given CAIP-10 identity.
    /// The account name is auto-generated from the address suffix.
    pub async fn create_with_pubkey(&self, pubkey: &str) -> Result<account::Model, DbErr> {
        let now = Utc::now().naive_utc();
        // Extract the address portion from CAIP-10 (last segment after ':').
        let addr = pubkey.rsplit(':').next().unwrap_or(pubkey);
        let suffix_len = 16.min(addr.len());
        let suffix = &addr[addr.len() - suffix_len..];
        let name = format!("account-{suffix}");
        let model = account::ActiveModel {
            id: Set(Uuid::new_v4()),
            name: Set(name),
            caip10_identity: Set(Some(pubkey.to_owned())),
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
        if let Some(old_pubkey) = &account.caip10_identity {
            let now = Utc::now().naive_utc();
            let rotated = rotated_pubkey::ActiveModel {
                id: Set(Uuid::new_v4()),
                account_id: Set(account_id.0),
                pubkey: Set(old_pubkey.clone()),
                rotated_at: Set(now),
            };
            rotated_pubkey::Entity::insert(rotated)
                .on_conflict(
                    sea_orm::sea_query::OnConflict::column(rotated_pubkey::Column::Pubkey)
                        .do_nothing()
                        .to_owned(),
                )
                .exec(self.db)
                .await?;
        }

        // Update the account with the new pubkey.
        let now = Utc::now().naive_utc();
        let mut active: account::ActiveModel = account.into();
        active.caip10_identity = Set(Some(new_pubkey.to_owned()));
        active.updated_at = Set(now);
        let updated = active.update(self.db).await?;

        Ok(Some(updated))
    }
}
