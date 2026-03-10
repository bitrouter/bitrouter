use chrono::Utc;
use sea_orm::{prelude::Expr, *};
use uuid::Uuid;

use crate::entity::{message, session};
use crate::identity::AccountId;

/// Session and message data operations.
pub struct SessionService<'db> {
    db: &'db DatabaseConnection,
}

impl<'db> SessionService<'db> {
    pub fn new(db: &'db DatabaseConnection) -> Self {
        Self { db }
    }

    // ── sessions ──────────────────────────────────────────────

    pub async fn create_session(
        &self,
        account_id: AccountId,
        title: Option<&str>,
    ) -> Result<session::Model, DbErr> {
        let now = Utc::now().naive_utc();
        let model = session::ActiveModel {
            id: Set(Uuid::new_v4()),
            account_id: Set(account_id.0),
            title: Set(title.map(str::to_owned)),
            created_at: Set(now),
            updated_at: Set(now),
        };
        model.insert(self.db).await
    }

    pub async fn get_session(&self, session_id: Uuid) -> Result<Option<session::Model>, DbErr> {
        session::Entity::find_by_id(session_id).one(self.db).await
    }

    pub async fn list_sessions(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<session::Model>, DbErr> {
        session::Entity::find()
            .filter(session::Column::AccountId.eq(account_id.0))
            .order_by_desc(session::Column::UpdatedAt)
            .all(self.db)
            .await
    }

    pub async fn delete_session(&self, session_id: Uuid) -> Result<(), DbErr> {
        session::Entity::delete_by_id(session_id)
            .exec(self.db)
            .await?;
        Ok(())
    }

    // ── messages ──────────────────────────────────────────────

    /// Append a message to a session. The `payload` is a JSON-serialized
    /// [`LanguageModelMessage`](bitrouter_core::models::language::prompt::LanguageModelMessage).
    pub async fn append_message(
        &self,
        session_id: Uuid,
        role: &str,
        payload: &str,
    ) -> Result<message::Model, DbErr> {
        let now = Utc::now().naive_utc();

        // Determine next position.
        let max_pos: Option<i32> = message::Entity::find()
            .filter(message::Column::SessionId.eq(session_id))
            .select_only()
            .column_as(message::Column::Position.max(), "max_pos")
            .into_tuple()
            .one(self.db)
            .await?;
        let position = max_pos.map_or(0, |p| p + 1);

        let model = message::ActiveModel {
            id: Set(Uuid::new_v4()),
            session_id: Set(session_id),
            position: Set(position),
            role: Set(role.to_owned()),
            payload: Set(payload.to_owned()),
            created_at: Set(now),
        };

        // Touch the session's updated_at.
        session::Entity::update_many()
            .filter(session::Column::Id.eq(session_id))
            .col_expr(session::Column::UpdatedAt, Expr::value(now))
            .exec(self.db)
            .await?;

        model.insert(self.db).await
    }

    pub async fn list_messages(
        &self,
        session_id: Uuid,
    ) -> Result<Vec<message::Model>, DbErr> {
        message::Entity::find()
            .filter(message::Column::SessionId.eq(session_id))
            .order_by_asc(message::Column::Position)
            .all(self.db)
            .await
    }
}
