//! A SQL allocation phase ends at worker acknowledgement, not stream Drop.
use super::{Charge, Workspace};
use anyhow::Result;
use sea_orm::sqlx::{Connection, SqliteConnection};

pub(super) struct Phase(Option<Charge>);
impl Phase {
    pub(super) fn new(workspace: &Workspace, bytes: usize) -> Result<Self> {
        Ok(Self(Some(workspace.reserve(bytes)?)))
    }
    pub(super) fn grow(&mut self, bytes: usize) -> Result<()> {
        self.0
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("SQL phase already settled"))?
            .grow(bytes)
    }
    pub(super) async fn clear(mut self, db: &mut SqliteConnection) -> Result<()> {
        // SQLx's ClearCache is queued after Execute and replies after clearing
        // both cached and temp statements. Dropping Execute only resets temp.
        // https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-sqlite/src/connection/worker.rs
        db.clear_cached_statements().await?;
        drop(self.0.take());
        Ok(())
    }
}
impl Drop for Phase {
    fn drop(&mut self) {
        if let Some(charge) = self.0.take() {
            // A lost/failed ACK cannot free working allowance while the worker
            // may retain its statement or rows. Same process-exit quarantine as
            // the connection owner's unconfirmed cleanup; never claim capacity.
            std::mem::forget(charge);
            tracing::error!("offline SQL phase cleanup unconfirmed; working allowance retained");
        }
    }
}
