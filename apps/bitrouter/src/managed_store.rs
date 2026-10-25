//! The native managed harness uses the same host database as BRO's other state.
//! A checkpoint, exact ACK, artifact closure and tool-start tombstones occupy
//! one versioned row, so no partial combination can become execution authority.

use crate::harness::resources::managed::store::{MAX_STORE_BYTES, NativeStore};
use bitrouter_ai::core::protocol::{CoreError, ErrorCode};
use sea_orm::{
    AccessMode, ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend,
    DatabaseConnection, EntityTrait, IsolationLevel, QueryFilter, QuerySelect, Set,
    TransactionTrait,
};

#[cfg(feature = "core")]
pub(crate) mod owned;

pub struct DatabaseNativeStore {
    db: DatabaseConnection,
    id: String,
}

impl DatabaseNativeStore {
    pub fn new(db: DatabaseConnection, id: String) -> anyhow::Result<Self> {
        crate::managed_task::protocol::validate_session_name(&id)?;
        Ok(Self { db, id })
    }
}

mod session {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "bro_managed_sessions")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub revision: i64,
        pub payload_bytes: i64,
        pub payload: String,
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
        pub session_principal_id: Option<String>,
        pub logical_name: Option<String>,
        pub harness_id: Option<String>,
        pub session_id: Option<String>,
        pub execution_revision: Option<String>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

#[async_trait::async_trait]
impl NativeStore for DatabaseNativeStore {
    async fn load(&self) -> Result<Option<(u64, Vec<u8>)>, CoreError> {
        // Bound the payload before fetching it, while keeping both SELECTs in
        // the same snapshot if the live owner publishes another revision.
        let (isolation, access) = match self.db.get_database_backend() {
            DatabaseBackend::Sqlite => (None, None),
            _ => (
                Some(IsolationLevel::RepeatableRead),
                Some(AccessMode::ReadOnly),
            ),
        };
        let transaction = self
            .db
            .begin_with_config(isolation, access)
            .await
            .map_err(storage)?;
        let metadata = session::Entity::find_by_id(&self.id)
            .filter(legacy())
            .select_only()
            .columns([session::Column::Revision, session::Column::PayloadBytes])
            .into_tuple::<(i64, i64)>()
            .one(&transaction)
            .await
            .map_err(storage)?;
        let Some((revision, bytes)) = metadata else {
            transaction.commit().await.map_err(storage)?;
            return Ok(None);
        };
        if revision <= 0 || bytes < 0 || bytes as u64 > MAX_STORE_BYTES as u64 {
            return Err(storage("invalid or oversized managed session"));
        }
        let record = session::Entity::find_by_id(&self.id)
            .filter(legacy())
            .filter(session::Column::Revision.eq(revision))
            .one(&transaction)
            .await
            .map_err(storage)?
            .ok_or_else(|| storage("managed session changed while reading"))?;
        if record.payload.len() as i64 != bytes {
            return Err(storage("managed payload length mismatch"));
        }
        transaction.commit().await.map_err(storage)?;
        Ok(Some((revision as u64, record.payload.into_bytes())))
    }

    async fn save(&self, expected_revision: u64, bytes: Vec<u8>) -> Result<(), CoreError> {
        if bytes.len() > MAX_STORE_BYTES {
            return Err(storage("managed payload exceeds storage bound"));
        }
        let previous = i64::try_from(expected_revision).map_err(storage)?;
        let next = previous
            .checked_add(1)
            .ok_or_else(|| storage("managed revision exhausted"))?;
        let length = bytes.len() as i64;
        let payload = String::from_utf8(bytes).map_err(storage)?;
        if previous == 0 {
            session::ActiveModel {
                id: Set(self.id.clone()),
                revision: Set(next),
                payload_bytes: Set(length),
                payload: Set(payload),
                installation_id: Set(None),
                tenant_id: Set(None),
                creator_principal_id: Set(None),
                session_principal_id: Set(None),
                logical_name: Set(None),
                harness_id: Set(None),
                session_id: Set(None),
                execution_revision: Set(None),
            }
            .insert(&self.db)
            .await
            .map_err(storage)?;
        } else {
            let result = session::Entity::update_many()
                .col_expr(
                    session::Column::Revision,
                    sea_orm::sea_query::Expr::value(next),
                )
                .col_expr(
                    session::Column::PayloadBytes,
                    sea_orm::sea_query::Expr::value(length),
                )
                .col_expr(
                    session::Column::Payload,
                    sea_orm::sea_query::Expr::value(payload),
                )
                .filter(legacy())
                .filter(session::Column::Id.eq(&self.id))
                .filter(session::Column::Revision.eq(previous))
                .exec(&self.db)
                .await
                .map_err(storage)?;
            if result.rows_affected != 1 {
                return Err(storage("managed session ownership revision changed"));
            }
        }
        Ok(())
    }
}

fn storage(error: impl std::fmt::Display) -> CoreError {
    CoreError::rejected(
        ErrorCode::CheckpointUnavailable,
        format!("managed session storage: {error}"),
    )
}

fn legacy() -> sea_orm::Condition {
    sea_orm::Condition::all()
        .add(session::Column::InstallationId.is_null())
        .add(session::Column::TenantId.is_null())
        .add(session::Column::CreatorPrincipalId.is_null())
        .add(session::Column::SessionPrincipalId.is_null())
        .add(session::Column::LogicalName.is_null())
        .add(session::Column::HarnessId.is_null())
        .add(session::Column::SessionId.is_null())
        .add(session::Column::ExecutionRevision.is_null())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn payload(revision: u64) -> Vec<u8> {
        format!(
            "revision:{revision}:{}",
            "x".repeat((revision as usize % 17) * 19)
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn snapshot_reads_keep_bounded_metadata_and_payload_coherent_during_live_commits()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let db = crate::db::connect(&format!(
            "sqlite://{}?mode=rwc",
            root.path().join("state.db").display()
        ))
        .await?;
        crate::db::run_migrations(&db).await?;
        let store = Arc::new(DatabaseNativeStore::new(db, "coherent-reader".into())?);
        store.save(0, payload(1)).await?;
        let writer = store.clone();
        let task = tokio::spawn(async move {
            for revision in 2..=256 {
                writer.save(revision - 1, payload(revision)).await?;
                tokio::task::yield_now().await;
            }
            Ok::<_, CoreError>(())
        });
        for _ in 0..256 {
            let (revision, bytes) = store
                .load()
                .await?
                .ok_or_else(|| anyhow::anyhow!("original session disappeared"))?;
            assert_eq!(bytes, payload(revision));
            tokio::task::yield_now().await;
        }
        task.await??;
        assert_eq!(store.load().await?, Some((256, payload(256))));
        Ok(())
    }
}
