//! Portable, compare-and-set persistence for jobs, assignments and block state.

use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction,
    EntityTrait, QueryFilter, QuerySelect, Set, TransactionTrait,
};
use serde::{Serialize, de::DeserializeOwned};

use sha2::{Digest, Sha256};

pub fn digest(value: &impl Serialize) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}

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
        self.decode_record(kind, row)
    }

    /// Read a mutation-time fence after its owning connection/session locks.
    /// Missing records remain meaningful; never create a row to validate it.
    pub(crate) async fn get_current_in<T: DeserializeOwned>(
        &self,
        tx: &DatabaseTransaction,
        kind: &str,
        key: &str,
    ) -> Result<Option<(i64, T)>> {
        // Ordinary reads can retain an earlier MySQL RR snapshot even after
        // the caller has acquired its authority row locks.
        // https://dev.mysql.com/doc/refman/8.4/en/innodb-locking-reads.html
        let row = records::Entity::find_by_id(self.id(kind, key)?)
            .lock_exclusive()
            .one(tx)
            .await?;
        self.decode_record(kind, row)
    }

    fn decode_record<T: DeserializeOwned>(
        &self,
        kind: &str,
        row: Option<records::Model>,
    ) -> Result<Option<(i64, T)>> {
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
        // RecordId is the sole unique key. MySQL needs a self-assignment
        // polyfill; retain the first writer's scope, body and revision rather
        // than replacing them with the retry's values.
        // https://docs.rs/sea-query/0.32.7/sea_query/query/struct.OnConflict.html#method.do_nothing_on
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
                .do_nothing_on([records::Column::RecordId])
                .to_owned(),
        )
        .do_nothing()
        .exec(db)
        .await?;
        // A prior consistent read may predate the winning insert. Read the
        // current locked row instead of that MySQL repeatable-read snapshot.
        // https://dev.mysql.com/doc/refman/8.4/en/innodb-locking-reads.html
        let row = records::Entity::find_by_id(self.id(kind, key)?)
            .lock_exclusive()
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
        // Retain the no-op write above for SQLite. MySQL must also read the
        // current revision after locking, even if this transaction read earlier.
        // https://dev.mysql.com/doc/refman/8.4/en/innodb-locking-reads.html
        let row = records::Entity::find_by_id(&id)
            .lock_exclusive()
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

#[cfg(all(test, feature = "db-mysql"))]
mod mysql_tests {
    use super::*;
    use sea_orm::IsolationLevel;
    use serde_json::{Value, json};

    async fn test_database() -> Result<DatabaseConnection> {
        let url = std::env::var("BITROUTER_TEST_DATABASE_URL")
            .context("set BITROUTER_TEST_DATABASE_URL to a dedicated MySQL test database")?;
        let parsed = url::Url::parse(&url)?;
        ensure!(
            parsed.scheme() == "mysql" && !parsed.path().trim_matches('/').is_empty(),
            "this regression requires an explicitly named MySQL test database"
        );
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        Ok(db)
    }

    #[tokio::test]
    #[ignore = "requires a dedicated MySQL BITROUTER_TEST_DATABASE_URL"]
    async fn initialization_reads_the_winner_after_an_older_snapshot() -> Result<()> {
        let db = test_database().await?;
        let result = async {
            let owner = format!("mysql-initialize-{}", uuid::Uuid::new_v4());
            let store = EvolutionStore::new(db.clone(), &owner)?;
            let reader = db
                .begin_with_config(Some(IsolationLevel::RepeatableRead), None)
                .await?;
            ensure!(
                store
                    .get_in::<Value>(&reader, "snapshot", "record")
                    .await?
                    .is_none()
            );
            let winner = json!({"value": "first writer"});
            store
                .initialize("snapshot", "record", None, &winner)
                .await?;
            let observed = store
                .initialize_in(
                    &reader,
                    "snapshot",
                    "record",
                    None,
                    &json!({"value": "retry"}),
                )
                .await?;
            ensure!(
                observed == (0, winner),
                "initialization read an obsolete snapshot"
            );
            reader.rollback().await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        let closed = db.close().await;
        result?;
        closed?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires a dedicated MySQL BITROUTER_TEST_DATABASE_URL"]
    async fn locked_record_reads_the_current_revision_after_an_older_snapshot() -> Result<()> {
        let db = test_database().await?;
        let result = async {
            let owner = format!("mysql-lock-{}", uuid::Uuid::new_v4());
            let store = EvolutionStore::new(db.clone(), &owner)?;
            store
                .initialize("snapshot", "record", None, &json!({"revision": 0}))
                .await?;
            let reader = db
                .begin_with_config(Some(IsolationLevel::RepeatableRead), None)
                .await?;
            ensure!(
                store.get_in::<Value>(&reader, "snapshot", "record").await?
                    == Some((0, json!({"revision": 0})))
            );
            store
                .update("snapshot", "record", 0, |body: &mut Value| {
                    *body = json!({"revision": 1});
                    Ok(())
                })
                .await?;
            let (row, mut body) = store.lock::<Value>(&reader, "snapshot", "record").await?;
            ensure!(
                row.revision == 1 && body == json!({"revision": 1}),
                "locked record read an obsolete revision"
            );
            body["reader"] = true.into();
            ensure!(store.save(&reader, row, &body).await? == 2);
            reader.commit().await?;
            ensure!(
                store.get::<Value>("snapshot", "record").await? == Some((2, body)),
                "saving the locked record lost the concurrent writer's state"
            );
            Ok::<(), anyhow::Error>(())
        }
        .await;
        let closed = db.close().await;
        result?;
        closed?;
        Ok(())
    }
}
