use super::*;
use crate::tenancy::transaction::{Control, Disposition as TransactionDisposition, Proof};
use sea_orm::sea_query::Asterisk;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) async fn exercise(fixture: &mut Fixture) -> Result<()> {
    let known = auxiliary_candidate("committed-known")?;
    let pending = unknown("committed-unknown")?;
    let live = Arc::new(AtomicBool::new(true));
    let proof = Proof::new(live.clone());
    let control = Control::new(proof.clone());
    let tx = control.begin_write(&fixture.db).await?;
    ensure!(persist(&tx, &known).await? && persist(&tx, &pending).await?);
    let expected = quantities(&matches_audit(&tx).await?);
    ensure!(expected[0] > 0 && expected[5] == 1);
    proof.lose_next_commit_ack_for_test();
    let error = control
        .commit(tx)
        .await
        .err()
        .context("real successful COMMIT did not lose its ACK")?;
    ensure!(error.downcast_ref::<DbErr>().is_some_and(|error| matches!(error,DbErr::Custom(message) if message == "commit acknowledgement lost by test transport")),
        "failure did not come from the transport hook after actual SQL COMMIT: {error:#}");
    control.conclude();
    ensure!(proof.disposition() == TransactionDisposition::Ambiguous);
    ensure!(!live.load(Ordering::Acquire) && proof.ensure_clean().is_err());
    drop((control, proof));
    // Explicitly close the first connector and acquire a fresh physical owner.
    // This proves real COMMIT durability/replay after ACK loss; it does not
    // impersonate a process kill or claim actor-custody retention coverage.
    fixture.reopen().await?;
    for value in [&known, &pending] {
        let saved = lookup_candidate(
            &fixture.db,
            &value.receipt.installation_id,
            &value.receipt.tenant_id,
            &value.receipt.execution_id,
        )
        .await?
        .context("real COMMIT lost source bytes")?;
        same_completion(&saved, value)?;
    }
    for (table, count) in [
        ("tenancy_execution_receipts", 2),
        ("tenancy_usage_evidence", 2),
        ("tenancy_receipt_auxiliary", 1),
        ("tenancy_unknown_obligations", 1),
    ] {
        let query = Query::select()
            .from(Alias::new(table))
            .expr_as(Expr::col(Asterisk).count(), Alias::new("n"))
            .to_owned();
        let row = fixture
            .db
            .query_one(fixture.db.get_database_backend().build(&query))
            .await?
            .context("durable source count")?;
        ensure!(
            row.try_get::<i64>("", "n")? == count,
            "ACK loss duplicated {table}"
        );
    }
    ensure!(expected == quantities(&matches_audit(&fixture.db).await?));
    let live = Arc::new(AtomicBool::new(true));
    let proof = Proof::new(live.clone());
    let control = Control::new(proof.clone());
    let tx = control.begin_write(&fixture.db).await?;
    ensure!(!persist(&tx, &known).await? && !persist(&tx, &pending).await?);
    control.commit(tx).await?;
    ensure!(
        proof.disposition() == TransactionDisposition::Committed && live.load(Ordering::Acquire)
    );
    proof.ensure_clean()?;
    ensure!(expected == quantities(&matches_audit(&fixture.db).await?));
    fixture.reopen().await?;
    ensure!(expected == quantities(&matches_audit(&fixture.db).await?));
    Ok(())
}
