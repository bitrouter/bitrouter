use super::*;
use crate::tenancy::receipts::store::{self, Candidate, Disposition};
use bitrouter_ai::tenancy::{
    execution::{ExecutionReceipt, Outcome, Provenance, ReceiptState, Usage},
    operation::DispatchState,
};
use sea_orm::TransactionTrait;

fn unknown(id: &str) -> Result<Candidate> {
    Candidate::new(
        ExecutionReceipt {
            installation_id: "installation".try_into()?,
            tenant_id: "tenant".try_into()?,
            principal_id: "creator".try_into()?,
            execution_id: id.try_into()?,
            correlation_id: "correlation".try_into()?,
            execution_revision: "revision".try_into()?,
            state: ReceiptState::Unknown,
            outcome: Outcome::Unknown,
            attempts: Vec::new(),
            usage: Usage {
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
                provenance: Provenance::Unknown,
                complete: false,
            },
            recorded_at: "2026-10-25T00:00:00Z".try_into()?,
        },
        Disposition {
            dispatch: DispatchState::Unknown,
            statuses: Vec::new(),
            auxiliary_digest: None,
        },
    )
}

fn receipt_key(value: &Candidate) -> Key {
    Key {
        tenant: value.receipt.tenant_id.clone(),
        execution: value.receipt.execution_id.clone(),
    }
}

#[tokio::test]
async fn ordered_overlay_preserves_baseline_across_pages_and_transaction_outcomes() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let installation = "installation".try_into()?;
    let free = |_, _| Ok(());
    let tx = db.begin().await?;
    for index in 0..65 {
        let candidate = unknown(&format!("k-{index:04}"))?;
        ensure!(store::insert(&tx, &candidate).await?);
        drop(obligations::reconcile(&tx, &installation, &receipt_key(&candidate), &free).await?);
    }
    let original = obligations::validate(&tx, &installation, &free).await?;
    tx.commit().await?;

    // Insert before the first page, between pages and after the last page;
    // retain an unchanged existing key as a real zero-delta publication step.
    for committed in [false, true] {
        let tx = db.begin().await?;
        let mut fixed = BTreeMap::new();
        for id in ["a", "k-0031", "k-0031-x", "z"] {
            let candidate = unknown(id)?;
            let key = receipt_key(&candidate);
            let before = obligations::lookup(&tx, &installation, &key, &free).await?;
            ensure!(store::insert(&tx, &candidate).await? == before.is_none());
            fixed.insert(key, before);
        }
        let check = || Ok(());
        let budget = OperationBudget::new(&check);
        let merge = merge_projection(&tx, &installation, &fixed, &budget).await?;
        ensure!(merge.baseline.finish() == original);
        let expected = merge.expected.finish();
        ensure!(expected.unknown_count == 68);
        ensure!(merge.changes.len() == 4);
        ensure!(
            merge
                .changes
                .iter()
                .filter(|change| change.delta() == 0)
                .count()
                == 1
        );
        ensure!(obligations::validate(&tx, &installation, &free).await? == expected);
        if committed {
            tx.commit().await?;
        } else {
            tx.rollback().await?;
        }
        let tx = db.begin().await?;
        ensure!(
            obligations::validate(&tx, &installation, &free).await?
                == if committed {
                    expected
                } else {
                    original.clone()
                }
        );
        tx.rollback().await?;
    }
    db.close().await?;
    Ok(())
}

#[test]
fn fold_has_no_historical_row_ceiling_and_retains_only_changed_keys() -> Result<()> {
    let installation = "installation".try_into()?;
    let mut merge = Merge::new(1)?;
    let mut baseline = Fold::new();
    let mut expected = Fold::new();
    let first =
        chrono::DateTime::parse_from_rfc3339("2026-10-25T00:00:00Z")?.with_timezone(&chrono::Utc);
    for index in 0..1007 {
        let row = Record {
            key: Key {
                tenant: "tenant".try_into()?,
                execution: format!("execution-{index:04}").try_into()?,
            },
            source_mask: 4,
            first,
            due: first + chrono::Duration::minutes(15),
            escalated: false,
        };
        baseline.push(&installation, &row)?;
        if index == 256 {
            let mut after = row.clone();
            after.source_mask = 5;
            expected.push(&installation, &after)?;
            merge.changed(
                &installation,
                Change {
                    key: row.key.clone(),
                    before: Some(row),
                    after: Some(after),
                },
            )?;
        } else {
            expected.push(&installation, &row)?;
            merge.unchanged(&installation, &row)?;
        }
    }
    let actual_baseline = merge.baseline.finish();
    let actual_expected = merge.expected.finish();
    ensure!(actual_baseline == baseline.finish());
    ensure!(actual_expected == expected.finish());
    ensure!(
        actual_expected.unknown_count == 1007 && actual_expected.digest != actual_baseline.digest
    );
    ensure!(merge.changes.len() == 1 && merge.changes[0].delta() == 0);
    Ok(())
}

#[test]
fn missing_receipt_charges_only_canonical_and_inline_bytes_atomically() -> Result<()> {
    let original = Inventory {
        technical_bytes: 0,
        bytes: 100,
        rows: 3,
        live_bytes: 200,
        history_bytes: 300,
        history_rows: 2,
        oauth_bytes: 400,
        unknown_count: 7,
        unknown_digest: [9; 32],
    };
    let mut expected = original.clone();
    add_missing_receipt(&mut expected, 23)?;
    ensure!(
        expected.bytes == 123 && expected.rows == 4 && expected.live_bytes == 223 + ROW_OVERHEAD
    );
    ensure!(
        expected.history_bytes == original.history_bytes
            && expected.history_rows == original.history_rows
            && expected.oauth_bytes == original.oauth_bytes
            && expected.unknown_count == original.unknown_count
            && expected.unknown_digest == original.unknown_digest
    );
    for field in 0..3 {
        let mut overflow = original.clone();
        match field {
            0 => overflow.rows = u64::MAX,
            1 => overflow.bytes = u64::MAX,
            _ => overflow.live_bytes = u64::MAX - 23,
        }
        let before = (overflow.rows, overflow.bytes, overflow.live_bytes);
        ensure!(add_missing_receipt(&mut overflow, 23).is_err());
        ensure!((overflow.rows, overflow.bytes, overflow.live_bytes) == before);
    }
    Ok(())
}
