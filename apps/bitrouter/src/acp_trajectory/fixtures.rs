//! Persisted capture fixtures for offline readers. No ACP runtime or capture port.
//!
//! Callers provide the session binding explicitly, including responses; these
//! helpers do not implement live call tracking, replay detection or write fences.
//! Capture envelopes follow https://agentclientprotocol.com/protocol/schema.

use std::sync::Arc;

use anyhow::{Context, Result};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
    TransactionTrait,
};

use crate::storage::capture::{CaptureEvent, CaptureKind, connections, events, sessions};
use crate::storage::session_identity::SessionIdentity;

pub(crate) struct FixtureScope {
    pub owner: String,
    pub source: String,
    pub controller_instance_id: Option<String>,
    pub route_scope_id: Option<String>,
}

pub(crate) struct PersistedHistory {
    db: DatabaseConnection,
    scope: FixtureScope,
    connection_id: String,
}

impl PersistedHistory {
    pub(crate) async fn new(db: DatabaseConnection, scope: FixtureScope) -> Result<Arc<Self>> {
        let connection_id = uuid::Uuid::new_v4().to_string();
        connections::ActiveModel {
            connection_id: Set(connection_id.clone()),
            owner: Set(scope.owner.clone()),
            source: Set(scope.source.clone()),
            controller_instance_id: Set(scope.controller_instance_id.clone()),
            route_scope_id: Set(scope.route_scope_id.clone()),
            state: Set("recording".into()),
            head: Set(0),
            started_at: Set(chrono::Utc::now().to_rfc3339()),
            metadata_json: Set("{}".into()),
        }
        .insert(&db)
        .await?;
        Ok(Arc::new(Self {
            db,
            scope,
            connection_id,
        }))
    }

    pub(crate) fn connection_id(&self) -> &str {
        &self.connection_id
    }

    pub(crate) async fn append(&self, native_session_id: &str, event: CaptureEvent) -> Result<()> {
        let tx = self.db.begin().await?;
        connections::Entity::update_many()
            .col_expr(
                connections::Column::Head,
                Expr::col(connections::Column::Head).add(1),
            )
            .filter(connections::Column::ConnectionId.eq(self.connection_id()))
            .exec(&tx)
            .await?;
        let connection = connections::Entity::find_by_id(self.connection_id())
            .one(&tx)
            .await?
            .context("fixture connection missing")?;
        let unbound = (event.kind == CaptureKind::Request && event.method == "session/new")
            || matches!(
                event.kind,
                CaptureKind::Connected | CaptureKind::Disconnected
            );
        let (session_key, session_sequence) = if unbound {
            (None, None)
        } else {
            let identity = SessionIdentity {
                owner: self.scope.owner.clone(),
                source: self.scope.source.clone(),
                native_session_id: native_session_id.into(),
            };
            let key = identity.key()?;
            let sequence =
                if let Some(session) = sessions::Entity::find_by_id(&key).one(&tx).await? {
                    sessions::Entity::update_many()
                        .col_expr(sessions::Column::Head, Expr::value(session.head + 1))
                        .filter(sessions::Column::SessionKey.eq(&key))
                        .exec(&tx)
                        .await?;
                    session.head + 1
                } else {
                    sessions::ActiveModel {
                        session_key: Set(key.clone()),
                        owner: Set(identity.owner),
                        source: Set(identity.source),
                        native_session_id: Set(identity.native_session_id),
                        head: Set(1),
                        deleted: Set(false),
                        history_origin: Set(if event.method == "session/new" {
                            "new"
                        } else {
                            "partial"
                        }
                        .into()),
                        parent_key: Set(None),
                        parent_watermark: Set(None),
                    }
                    .insert(&tx)
                    .await?;
                    1
                };
            (Some(key), Some(sequence))
        };
        if event.kind == CaptureKind::Disconnected {
            connections::Entity::update_many()
                .col_expr(
                    connections::Column::State,
                    Expr::value(
                        if event
                            .payload
                            .get("clean")
                            .and_then(serde_json::Value::as_bool)
                            == Some(true)
                        {
                            "complete"
                        } else {
                            "interrupted"
                        },
                    ),
                )
                .filter(connections::Column::ConnectionId.eq(self.connection_id()))
                .exec(&tx)
                .await?;
        }
        events::ActiveModel {
            connection_id: Set(self.connection_id().into()),
            sequence: Set(connection.head),
            session_key: Set(session_key),
            session_sequence: Set(session_sequence),
            replay: Set(false),
            event_json: Set(serde_json::to_string(&event)?),
            captured_at: Set(chrono::Utc::now().to_rfc3339()),
        }
        .insert(&tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
