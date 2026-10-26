//! Lost-ACK reconciliation derives the event edge from finite durable history.
use super::{
    Append, Association, MAX_CAPTURE_BYTES, MAX_EVENTS, entities::events, io, protocol::State,
};
use crate::storage::operation_budget::OperationBudget;
use anyhow::{Context, Result, ensure};
use sea_orm::{DatabaseTransaction, QueryOrder};

impl Association {
    /// Read-only even on malformed closure. The stored edge is evidence to
    /// compare, never an input deciding which session must exist for this ACK.
    pub(super) async fn check_replay_in(
        &self,
        tx: &DatabaseTransaction,
        append: &Append,
        current: &State,
        budget: &OperationBudget<'_>,
    ) -> Result<()> {
        let rows = io::rows(
            tx,
            self.events().order_by_asc(events::Column::Sequence),
            MAX_EVENTS,
            MAX_CAPTURE_BYTES + 4096 * MAX_EVENTS,
            budget,
        )
        .await?;
        ensure!(
            i64::try_from(rows.len())? == append.sequence,
            "capture replay prefix has a gap"
        );
        let last = rows.last().context("capture replay event absent")?;
        ensure!(
            last.sequence == append.sequence
                && last.event_json == append.bytes
                && last.captured_at == append.at,
            "capture replay mismatch"
        );
        let mut state = State::initial(self);
        for row in rows {
            budget.check()?;
            self.validate(
                row.installation_id.as_deref(),
                row.tenant_id.as_deref(),
                row.creator_principal_id.as_deref(),
            )?;
            ensure!(!row.replay, "fresh capture cannot contain imported replay");
            let event = serde_json::from_str(&row.event_json)?;
            let retained = Append::new(row.sequence, row.captured_at, event)?;
            ensure!(
                retained.bytes == row.event_json,
                "capture event encoding changed"
            );
            let (next, native, _) = state.next(&retained)?;
            let key = native
                .as_deref()
                .map(|id| self.session_key(id))
                .transpose()?;
            let head = native
                .as_ref()
                .and_then(|id| next.sessions.get(id).copied());
            ensure!(
                row.session_key == key && row.session_sequence == head,
                "capture replay session edge changed"
            );
            state = next;
        }
        ensure!(state == *current, "capture replay correlation changed");
        for (native, head) in &state.sessions {
            let session = self.session_in(tx, native, budget).await?;
            ensure!(
                session.head == *head,
                "capture replay session closure changed"
            );
        }
        budget.check()?;
        Ok(())
    }
}
