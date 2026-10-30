use super::{
    Append, Appended, Association, MAX_METADATA_BYTES,
    entities::{connections, events, sessions},
    io,
    protocol::State,
};
use crate::storage::operation_budget::OperationBudget;
use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set};

impl Association {
    pub(crate) async fn check_cleanup_in(
        &self,
        tx: &DatabaseTransaction,
        append: &Append,
        budget: &OperationBudget<'_>,
    ) -> Result<()> {
        use crate::storage::capture::{CaptureDirection, CaptureKind};
        let (_, state) = self.connection_in(tx, budget).await?;
        let event = &append.event;
        let allowed = match event.kind {
            CaptureKind::Response => event
                .call_id
                .is_some_and(|id| state.calls.contains_key(&id)),
            CaptureKind::Disconnected => {
                event.direction == CaptureDirection::Controller
                    && !state.closed
                    && (state.connected || state.head == 0)
            }
            CaptureKind::Notification => {
                event
                    .payload
                    .get("sessionId")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|native| {
                        state.calls.values().any(|call| {
                            call.method == "session/prompt"
                                && call.session.as_deref() == Some(native)
                        })
                    })
                    && matches!(
                        (event.direction, event.method.as_str()),
                        (CaptureDirection::Agent, "session/update")
                            | (CaptureDirection::Client, "session/cancel")
                    )
            }
            _ => false,
        };
        ensure!(allowed, super::InvalidObservation);
        Ok(())
    }

    /// Initial connection insert participates in the actor transaction. A
    /// duplicate must already be exact; it never changes existing metadata.
    pub(crate) async fn create_in(
        &self,
        tx: &DatabaseTransaction,
        at: &str,
        budget: &OperationBudget<'_>,
    ) -> Result<()> {
        budget.check()?;
        ensure!(at.len() <= 64, "capture timestamp exceeds bound");
        chrono::DateTime::parse_from_rfc3339(at)?;
        let query = connections::Entity::find_by_id(self.connection())
            .filter(self.predicate("acp_capture_connections"));
        if let Some(row) = io::one(tx, query, MAX_METADATA_BYTES + 4096, budget).await? {
            self.validate(
                row.installation_id.as_deref(),
                row.tenant_id.as_deref(),
                row.creator_principal_id.as_deref(),
            )?;
            let state: State = serde_json::from_str(&row.metadata_json)?;
            state.validate(self, &row)?;
            ensure!(
                state == State::initial(self)
                    && row.started_at == at
                    && row.owner == self.descriptor.principal
                    && row.source == self.descriptor.source
                    && row.controller_instance_id.as_deref()
                        == Some(self.descriptor.controller.as_str())
                    && row.route_scope_id.is_none(),
                "capture initial row conflicts"
            );
            return Ok(());
        }
        let metadata = self.metadata()?;
        let json = io::json(&State::initial(self), MAX_METADATA_BYTES)?;
        budget.charge(1, json.len() + 4096)?;
        let inserted = connections::Entity::insert(connections::ActiveModel {
            connection_id: Set(self.connection.clone()),
            owner: Set(self.descriptor.principal.clone()),
            source: Set(self.descriptor.source.clone()),
            controller_instance_id: Set(Some(self.descriptor.controller.clone())),
            route_scope_id: Set(None),
            state: Set("recording".into()),
            head: Set(0),
            started_at: Set(at.to_owned()),
            metadata_json: Set(json),
            installation_id: Set(metadata.installation),
            tenant_id: Set(metadata.tenant),
            creator_principal_id: Set(metadata.creator),
        })
        .exec_without_returning(tx)
        .await?;
        ensure!(inserted == 1, "capture connection insert missing");
        budget.check()?;
        Ok(())
    }

    pub(crate) async fn append_in(
        &self,
        tx: &DatabaseTransaction,
        append: &Append,
        budget: &OperationBudget<'_>,
    ) -> Result<Appended> {
        let (connection, state) = self.connection_in(tx, budget).await?;
        if connection.head == append.sequence {
            // Reconcile a lost ACK read-only. Missing/mutated closure refuses;
            // replay cannot manufacture an event or repair its session row.
            self.check_replay_in(tx, append, &state, budget).await?;
            budget.check()?;
            return Ok(Appended {
                head: append.sequence,
                duplicate: true,
            });
        }
        let (next, native, created) = state.next(append).map_err(|_| super::InvalidObservation)?;
        let json = io::json(&next, MAX_METADATA_BYTES)?;
        let metadata = self.metadata()?;
        budget.charge(3, json.len() + append.bytes.len() + 8192)?;
        let (session_key, session_head) = if let Some(native) = &native {
            let key = self.session_key(native)?;
            let head = *next
                .sessions
                .get(native)
                .context("capture next session is absent")?;
            if created {
                // Strict INSERT: a foreign, Legacy, partial or reused physical
                // key is a failure. No upsert can turn it into this namespace.
                let inserted = sessions::Entity::insert(sessions::ActiveModel {
                    session_key: Set(key.clone()),
                    owner: Set(self.descriptor.principal.clone()),
                    source: Set(self.descriptor.source.clone()),
                    native_session_id: Set(native.clone()),
                    head: Set(head),
                    history_origin: Set("new".into()),
                    deleted: Set(false),
                    parent_key: Set(None),
                    parent_watermark: Set(None),
                    installation_id: Set(metadata.installation.clone()),
                    tenant_id: Set(metadata.tenant.clone()),
                    creator_principal_id: Set(metadata.creator.clone()),
                })
                .exec_without_returning(tx)
                .await?;
                ensure!(inserted == 1, "owned capture session insert missing");
            } else {
                let current = self.session_in(tx, native, budget).await?;
                ensure!(
                    state.sessions.get(native) == Some(&current.head)
                        && current.head.checked_add(1) == Some(head),
                    "capture session head changed"
                );
                budget.check()?;
                let updated = sessions::Entity::update_many()
                    .filter(self.predicate("acp_canonical_sessions"))
                    .filter(sessions::Column::SessionKey.eq(&key))
                    .filter(sessions::Column::Head.eq(current.head))
                    .filter(sessions::Column::Deleted.eq(false))
                    .col_expr(sessions::Column::Head, Expr::value(head))
                    .exec(tx)
                    .await?;
                ensure!(updated.rows_affected == 1, "capture session CAS failed");
            }
            (Some(key), Some(head))
        } else {
            (None, None)
        };
        budget.check()?;
        let inserted = events::Entity::insert(events::ActiveModel {
            connection_id: Set(self.connection.clone()),
            sequence: Set(append.sequence),
            session_key: Set(session_key),
            session_sequence: Set(session_head),
            replay: Set(false),
            event_json: Set(append.bytes.clone()),
            captured_at: Set(append.at.clone()),
            installation_id: Set(metadata.installation),
            tenant_id: Set(metadata.tenant),
            creator_principal_id: Set(metadata.creator),
        })
        .exec_without_returning(tx)
        .await?;
        ensure!(inserted == 1, "capture event insert missing");
        budget.check()?;
        let updated = connections::Entity::update_many()
            .filter(self.predicate("acp_capture_connections"))
            .filter(connections::Column::ConnectionId.eq(&self.connection))
            .filter(connections::Column::Head.eq(connection.head))
            .filter(connections::Column::State.eq("recording"))
            .col_expr(connections::Column::Head, Expr::value(next.head))
            .col_expr(connections::Column::MetadataJson, Expr::value(json))
            .col_expr(connections::Column::State, Expr::value(next.state_name()))
            .exec(tx)
            .await?;
        ensure!(updated.rows_affected == 1, "capture connection CAS failed");
        budget.check()?;
        Ok(Appended {
            head: next.head,
            duplicate: false,
        })
    }

    pub(crate) async fn session_in(
        &self,
        tx: &DatabaseTransaction,
        native: &str,
        budget: &OperationBudget<'_>,
    ) -> Result<sessions::Model> {
        let row = io::one(tx, self.sessions(native)?, 4096, budget)
            .await?
            .context("owned capture session unavailable")?;
        self.validate(
            row.installation_id.as_deref(),
            row.tenant_id.as_deref(),
            row.creator_principal_id.as_deref(),
        )?;
        ensure!(
            !row.deleted
                && row.history_origin == "new"
                && row.parent_key.is_none()
                && row.parent_watermark.is_none(),
            "capture session ancestry is unsupported"
        );
        Ok(row)
    }
}
