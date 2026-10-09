//! The native managed harness uses the same host database as BRO's other state.
//! A checkpoint, exact ACK, artifact closure and tool-start tombstones occupy
//! one versioned row, so no partial combination can become execution authority.

use bitrouter_orchestrator::core::protocol::{CoreError, ErrorCode};
use bitrouter_orchestrator::harness::managed::store::{MAX_STORE_BYTES, NativeStore};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QuerySelect, Set,
};

pub struct DatabaseNativeStore {
    db: DatabaseConnection,
    id: String,
}

impl DatabaseNativeStore {
    pub fn new(db: DatabaseConnection, id: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
            "invalid managed session name"
        );
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
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

#[async_trait::async_trait]
impl NativeStore for DatabaseNativeStore {
    async fn load(&self) -> Result<Option<(u64, Vec<u8>)>, CoreError> {
        let metadata = session::Entity::find_by_id(&self.id)
            .select_only()
            .columns([session::Column::Revision, session::Column::PayloadBytes])
            .into_tuple::<(i64, i64)>()
            .one(&self.db)
            .await
            .map_err(storage)?;
        let Some((revision, bytes)) = metadata else {
            return Ok(None);
        };
        if revision <= 0 || bytes < 0 || bytes as u64 > MAX_STORE_BYTES as u64 {
            return Err(storage("invalid or oversized managed session"));
        }
        let record = session::Entity::find_by_id(&self.id)
            .filter(session::Column::Revision.eq(revision))
            .one(&self.db)
            .await
            .map_err(storage)?
            .ok_or_else(|| storage("managed session changed while reading"))?;
        if record.payload.len() as i64 != bytes {
            return Err(storage("managed payload length mismatch"));
        }
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
