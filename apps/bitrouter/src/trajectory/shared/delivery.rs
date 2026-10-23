//! Delivery bookkeeping joins the receiver's receipt/evaluation transaction.

use super::*;

impl SharedTrajectory {
    /// Lock before evaluation rows. Marking and required admission become visible
    /// together at the caller's commit; any later failure must roll back both.
    pub async fn deliver_outbox_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        id: &str,
        permit_new: bool,
    ) -> Result<PendingOutbox> {
        check_transaction(tx).await?;
        validate_scope(owner, id)?;
        // https://www.postgresql.org/docs/current/explicit-locking.html#LOCKING-ROWS
        let row = tx.query_one(self.statement(tx,
            "SELECT namespace,delivered_at FROM gateway_shared_trajectory_outbox WHERE namespace=$1 AND owner_user_id=$2 AND outbox_id=$3 FOR UPDATE",
            vec![owner.into(), id.into()])?).await?.context("required trajectory outbox is missing")?;
        self.check_namespace(&row)?;
        let existing: Option<String> = row.try_get("", "delivered_at")?;
        let outbox = self
            .outbox_in(tx, owner, id)
            .await?
            .context("locked trajectory outbox disappeared")?;
        if let Some(delivered_at) = existing {
            chrono::DateTime::parse_from_rfc3339(&delivered_at)
                .context("invalid trajectory delivery timestamp")?;
        } else {
            ensure!(
                permit_new,
                "delivered receipt lost trajectory delivery mark"
            );
            // Database time owns delivery metadata, never semantic event identity.
            // https://www.postgresql.org/docs/current/functions-datetime.html
            let updated = tx.execute(self.statement(tx,
                "UPDATE gateway_shared_trajectory_outbox SET delivered_at=to_char(clock_timestamp() AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'),attempts=attempts+1 WHERE namespace=$1 AND owner_user_id=$2 AND outbox_id=$3 AND delivered_at IS NULL",
                vec![owner.into(), id.into()])?).await?;
            ensure!(
                updated.rows_affected() == 1,
                "trajectory delivery mark changed"
            );
        }
        Ok(outbox)
    }
}
