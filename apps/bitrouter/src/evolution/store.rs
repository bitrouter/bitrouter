//! Portable, compare-and-set persistence for jobs, assignments and block state.

use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction,
    EntityTrait, QueryFilter, Set, TransactionTrait,
};
use serde::{Serialize, de::DeserializeOwned};

use super::rubric::digest;

pub mod records {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "checkpoint_evolution")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub record_id: String,
        pub scope_id: String,
        pub kind: String,
        pub session_key: Option<String>,
        pub revision: i64,
        pub body: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

#[derive(Clone)]
pub struct EvolutionStore {
    pub(crate) db: DatabaseConnection,
    pub scope_id: String,
    pub(crate) owner: String,
}

impl EvolutionStore {
    pub fn new(db: DatabaseConnection, owner: &str) -> Result<Self> {
        ensure!(!owner.is_empty(), "evolution owner is required");
        Ok(Self {
            db,
            scope_id: digest(&("checkpoint-evolution-v1", owner))?,
            owner: owner.into(),
        })
    }

    pub fn ensure_owner(&self, owner: &str) -> Result<()> {
        ensure!(owner == self.owner, "evolution owner scope mismatch");
        Ok(())
    }

    pub fn id(&self, kind: &str, key: &str) -> Result<String> {
        digest(&(&self.scope_id, kind, key))
    }

    pub async fn get<T: DeserializeOwned>(
        &self,
        kind: &str,
        key: &str,
    ) -> Result<Option<(i64, T)>> {
        self.get_in(&self.db, kind, key).await
    }

    pub(crate) async fn get_in<T: DeserializeOwned>(
        &self,
        db: &impl ConnectionTrait,
        kind: &str,
        key: &str,
    ) -> Result<Option<(i64, T)>> {
        let row = records::Entity::find_by_id(self.id(kind, key)?)
            .one(db)
            .await?;
        row.map(|row| {
            ensure!(
                row.scope_id == self.scope_id && row.kind == kind,
                "evolution record scope mismatch"
            );
            Ok((row.revision, serde_json::from_str(&row.body)?))
        })
        .transpose()
    }

    pub async fn list<T: DeserializeOwned>(&self, kind: &str) -> Result<Vec<(String, i64, T)>> {
        records::Entity::find()
            .filter(records::Column::ScopeId.eq(&self.scope_id))
            .filter(records::Column::Kind.eq(kind))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| {
                Ok((
                    row.record_id,
                    row.revision,
                    serde_json::from_str(&row.body)?,
                ))
            })
            .collect()
    }

    /// Returns the persisted first writer. The caller must validate immutable
    /// input identity before treating an existing job/assignment as a retry.
    pub async fn initialize<T: Serialize + DeserializeOwned>(
        &self,
        kind: &str,
        key: &str,
        session_key: Option<String>,
        body: &T,
    ) -> Result<(i64, T)> {
        self.initialize_in(&self.db, kind, key, session_key, body)
            .await
    }

    pub(crate) async fn initialize_in<T: Serialize + DeserializeOwned>(
        &self,
        db: &impl ConnectionTrait,
        kind: &str,
        key: &str,
        session_key: Option<String>,
        body: &T,
    ) -> Result<(i64, T)> {
        records::Entity::insert(records::ActiveModel {
            record_id: Set(self.id(kind, key)?),
            scope_id: Set(self.scope_id.clone()),
            kind: Set(kind.into()),
            session_key: Set(session_key),
            revision: Set(0),
            body: Set(serde_json::to_string(body)?),
        })
        .on_conflict(
            OnConflict::column(records::Column::RecordId)
                .do_nothing()
                .to_owned(),
        )
        .do_nothing()
        .exec(db)
        .await?;
        let row = records::Entity::find_by_id(self.id(kind, key)?)
            .one(db)
            .await?
            .context("evolution record missing after initialization")?;
        ensure!(
            row.scope_id == self.scope_id && row.kind == kind,
            "evolution record scope mismatch"
        );
        Ok((row.revision, serde_json::from_str(&row.body)?))
    }

    /// Locks the record on every supported backend. No network/model work may
    /// execute in this closure. A stale caller cannot overwrite a newer epoch.
    pub async fn update<T, R>(
        &self,
        kind: &str,
        key: &str,
        expected: i64,
        change: impl FnOnce(&mut T) -> Result<R>,
    ) -> Result<(i64, R)>
    where
        T: Serialize + DeserializeOwned,
    {
        let tx = self.db.begin().await?;
        let (row, mut body): (_, T) = self.lock(&tx, kind, key).await?;
        ensure!(
            row.revision == expected,
            "evolution generation changed; refresh before retrying"
        );
        let result = change(&mut body)?;
        let revision = self.save(&tx, row, &body).await?;
        tx.commit().await?;
        Ok((revision, result))
    }

    /// Callers lock the control record before any assignment record. This is the
    /// shared serialization point for enrollment, mode changes and publication.
    pub(crate) async fn lock<T: DeserializeOwned>(
        &self,
        tx: &DatabaseTransaction,
        kind: &str,
        key: &str,
    ) -> Result<(records::Model, T)> {
        let id = self.id(kind, key)?;
        records::Entity::update_many()
            .col_expr(
                records::Column::Revision,
                Expr::col(records::Column::Revision).into(),
            )
            .filter(records::Column::RecordId.eq(&id))
            .exec(tx)
            .await?;
        let row = records::Entity::find_by_id(&id)
            .one(tx)
            .await?
            .context("evolution record not found")?;
        ensure!(
            row.scope_id == self.scope_id && row.kind == kind,
            "evolution record scope mismatch"
        );
        let body = serde_json::from_str(&row.body)?;
        Ok((row, body))
    }

    pub(crate) async fn save<T: Serialize>(
        &self,
        tx: &DatabaseTransaction,
        row: records::Model,
        body: &T,
    ) -> Result<i64> {
        ensure!(
            row.scope_id == self.scope_id,
            "evolution record scope mismatch"
        );
        let revision = row
            .revision
            .checked_add(1)
            .context("evolution generation overflow")?;
        let mut active: records::ActiveModel = row.into();
        active.revision = Set(revision);
        active.body = Set(serde_json::to_string(body)?);
        active.update(tx).await?;
        Ok(revision)
    }
}
