//! One indexed key page per maintenance turn; no history-wide eligibility scan.
use super::super::format::Terminal;
use super::*;
use crate::tenancy::receipts::store::obligations::{self, Key};

const PAGE_ROWS: usize = 32;

fn pending(scope: String, kind: &str) -> Statement {
    sqlite(
        "SELECT 1 FROM checkpoint_evolution INDEXED BY idx_checkpoint_evolution_scope_kind WHERE scope_id=? AND kind=? LIMIT 1",
        [scope.into(), kind.into()],
    )
}

/// Pending mirrors/units outlive receipt publication until their own ACKs.
/// Their existing index does not address a paid execution directly. Presence
/// conservatively holds this tenant's receipts; absence is a bounded exact seek,
/// independent of historical job count. No JSON bodies are loaded or scanned.
pub(super) async fn held(
    tx: &DatabaseTransaction,
    candidate: &Candidate,
    charge: &Charge<'_>,
) -> Result<bool> {
    let r = &candidate.receipt;
    charge(2, 2048)?;
    for (scope, kind) in [
        (
            crate::storage::partition::learning_pending(&r.installation_id, &r.tenant_id)?,
            "owned_judge_pending",
        ),
        (
            crate::storage::partition::native_experiment_pending(&r.installation_id, &r.tenant_id)?,
            "native_experiment_unit_pending_v1",
        ),
    ] {
        if tx.query_one(pending(scope, kind)).await?.is_some() {
            charge(0, 0)?;
            return Ok(true);
        }
    }
    charge(0, 0)?;
    Ok(false)
}

fn keys(installation: &InstallationId, after: Option<&Key>) -> Statement {
    let mut query = String::from(
        "SELECT CASE WHEN typeof(r.tenant_id)='text' AND length(CAST(r.tenant_id AS BLOB)) BETWEEN 1 AND 128 THEN r.tenant_id ELSE NULL END AS tenant_id, CASE WHEN typeof(r.execution_id)='text' AND length(CAST(r.execution_id AS BLOB)) BETWEEN 1 AND 128 THEN r.execution_id ELSE NULL END AS execution_id FROM tenancy_execution_receipts AS r INDEXED BY idx_tenancy_receipts_storage WHERE r.installation_id=? AND r.storage_kind=0",
    );
    let mut parameters = vec![installation.as_str().into()];
    if let Some(after) = after {
        // Seek the original composite index columns, never the CASE aliases.
        // https://sqlite.org/rowvalue.html#scrolling_window_queries
        query.push_str(" AND (r.tenant_id,r.execution_id)>(?,?)");
        parameters.extend([
            after.tenant.as_str().into(),
            after.execution.as_str().into(),
        ]);
    }
    query.push_str(" ORDER BY r.tenant_id,r.execution_id LIMIT 32");
    sqlite(query, parameters)
}

pub(super) async fn next(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    after: Option<&Key>,
    charge: &Charge<'_>,
) -> Result<Selection> {
    ensure!(
        tx.get_database_backend() == DatabaseBackend::Sqlite,
        "receipt archive requires SQLite"
    );
    // Includes SQL key cells, decoded identities, vector/cursor metadata. Each
    // canonical candidate is charged by its existing reader before allocation.
    charge(PAGE_ROWS, PAGE_ROWS * 1024)?;
    let page = tx.query_all(keys(installation, after)).await?;
    let count = page.len();
    ensure!(count <= PAGE_ROWS, "archive key page overflow");
    let mut cursor = after.cloned();
    for (index, row) in page.into_iter().enumerate() {
        let key = Key {
            tenant: row.try_get::<String>("", "tenant_id")?.try_into()?,
            execution: row.try_get::<String>("", "execution_id")?.try_into()?,
        };
        ensure!(
            cursor.as_ref().is_none_or(|previous| previous < &key),
            "archive key order"
        );
        // A validated durable source projection is required before maintenance.
        // Exclude pending owners cheaply; absence is never sufficient eligibility.
        let blocked = obligations::lookup(tx, installation, &key, charge)
            .await?
            .is_some();
        cursor = Some(key);
        if blocked {
            continue;
        }
        let key = cursor.as_ref().context("archive cursor missing")?;
        let candidate = super::super::load::terminal(
            tx,
            installation,
            &key.tenant,
            &key.execution,
            Some(charge),
            false,
        )
        .await?
        .context("archive candidate disappeared")?;
        let Terminal::V1(candidate) = candidate else {
            continue;
        };
        let candidate = std::sync::Arc::into_inner(candidate)
            .context("receipt shared during archive selection")?;
        let eligible = known(&candidate).is_ok() && !held(tx, &candidate, charge).await?;
        charge(0, 0)?;
        return Ok(Selection {
            candidate: eligible.then_some(candidate),
            after: cursor,
            done: count < PAGE_ROWS && index + 1 == count,
        });
    }
    charge(0, 0)?;
    Ok(Selection {
        candidate: None,
        after: cursor,
        done: count < PAGE_ROWS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenancy::receipts::store::{self, Disposition, tests::candidate};
    use bitrouter_ai::tenancy::{
        execution::{Outcome as ReceiptOutcome, ReceiptState},
        operation::DispatchState,
    };
    use sea_orm::{DatabaseConnection, TransactionTrait};

    const FREE: &Charge<'static> = &|_, _| Ok(());

    async fn open() -> Result<DatabaseConnection> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        Ok(db)
    }
    fn unknown(id: &str) -> Result<Candidate> {
        let mut receipt = candidate("installation", "tenant", id, "creator")?.receipt;
        receipt.state = ReceiptState::Unknown;
        receipt.outcome = ReceiptOutcome::Unknown;
        Candidate::new(
            receipt,
            Disposition {
                dispatch: DispatchState::Unknown,
                statuses: Vec::new(),
                auxiliary_digest: None,
            },
        )
    }
    fn key(id: &str) -> Result<Key> {
        Ok(Key {
            tenant: "tenant".try_into()?,
            execution: id.try_into()?,
        })
    }

    #[tokio::test]
    async fn keyset_query_uses_covering_storage_index_without_sorting_history() -> Result<()> {
        let db = open().await?;
        let installation = "installation".try_into()?;
        let cursor = key("after")?;
        for (query, index) in [
            (keys(&installation, None), "idx_tenancy_receipts_storage"),
            (
                keys(&installation, Some(&cursor)),
                "idx_tenancy_receipts_storage",
            ),
            (
                pending(
                    crate::storage::partition::learning_pending(&installation, &cursor.tenant)?,
                    "owned_judge_pending",
                ),
                "idx_checkpoint_evolution_scope_kind",
            ),
        ] {
            let explain = Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                format!("EXPLAIN QUERY PLAN {}", query.sql),
                query.values.context("query parameters")?.0,
            );
            let mut details = String::new();
            for row in db.query_all(explain).await? {
                details.push_str(&row.try_get::<String>("", "detail")?);
            }
            ensure!(
                details.contains("SEARCH ")
                    && details.contains(&format!("USING COVERING INDEX {index}")),
                "unexpected plan: {details}"
            );
            ensure!(
                !details.contains("TEMP B-TREE") && !details.contains("SCAN "),
                "history scan/sort: {details}"
            );
        }
        db.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn unknown_pages_and_pending_obligations_advance_to_late_eligible_row() -> Result<()> {
        let db = open().await?;
        let installation = "installation".try_into()?;
        let tx = db.begin().await?;
        for index in 0..PAGE_ROWS + 2 {
            let id = format!("a-{index:04}");
            store::insert(&tx, &unknown(&id)?).await?;
            ensure!(
                obligations::reconcile(&tx, &installation, &key(&id)?, FREE)
                    .await?
                    .delta()
                    == 1
            );
        }
        // A known receipt can still be held by unresolved OAuth work.
        let pending = candidate("installation", "tenant", "b-pending", "creator")?;
        store::insert(&tx, &pending).await?;
        let (_crypto, slot) = crate::tenancy::oauth::storage::pending::tests::fixture(
            &installation,
            &"tenant".try_into()?,
            &"b-pending".try_into()?,
            "selector",
            "2026-10-01T00:00:00Z",
        )?;
        crate::tenancy::oauth::storage::insert(&tx, &slot).await?;
        ensure!(
            obligations::reconcile(&tx, &installation, &key("b-pending")?, FREE)
                .await?
                .delta()
                == 1
        );
        let eligible = candidate("installation", "tenant", "z-eligible", "creator")?;
        store::insert(&tx, &eligible).await?;
        // Corruption beyond this call's key page cannot be inspected early.
        tx.execute_unprepared("UPDATE tenancy_execution_receipts SET content_digest='bad' WHERE execution_id='z-eligible'").await?;
        let first = next(&tx, &installation, None, FREE).await?;
        ensure!(first.candidate.is_none() && !first.done && first.after == Some(key("a-0031")?));
        ensure!(
            next(&tx, &installation, first.after.as_ref(), FREE)
                .await
                .is_err()
        );
        tx.execute(sqlite("UPDATE tenancy_execution_receipts SET content_digest=? WHERE execution_id='z-eligible'", [eligible.digest.as_str().into()])).await?;
        let last = next(&tx, &installation, first.after.as_ref(), FREE).await?;
        ensure!(last.done && last.after == Some(key("z-eligible")?));
        let found = last.candidate.context("late candidate missing")?;
        store::same_completion(&found, &eligible)?;
        ensure!(found.receipt.state == ReceiptState::Incomplete);
        tx.rollback().await?;
        db.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn canonical_unknowns_and_archived_rows_are_not_candidates_and_cursor_loses_no_keys()
    -> Result<()> {
        let db = open().await?;
        let installation = "installation".try_into()?;
        let tx = db.begin().await?;
        // Missing derived state still cannot make a canonical unknown eligible.
        store::insert(&tx, &unknown("a-unknown")?).await?;
        for id in ["b-archived", "c-first", "d-second"] {
            store::insert(&tx, &candidate("installation", "tenant", id, "creator")?).await?;
        }
        let archived = candidate("installation", "tenant", "b-archived", "creator")?;
        super::super::archive_one(&tx, &Prepared::new(&archived, FREE)?, FREE).await?;
        let unknown = next(&tx, &installation, None, FREE).await?;
        ensure!(
            unknown.candidate.is_none()
                && !unknown.done
                && unknown.after == Some(key("a-unknown")?)
        );
        let first = next(&tx, &installation, unknown.after.as_ref(), FREE).await?;
        ensure!(!first.done && first.after == Some(key("c-first")?));
        ensure!(
            first
                .candidate
                .context("first candidate")?
                .receipt
                .execution_id
                .as_str()
                == "c-first"
        );
        let second = next(&tx, &installation, first.after.as_ref(), FREE).await?;
        ensure!(second.done && second.after == Some(key("d-second")?));
        let empty = next(&tx, &installation, second.after.as_ref(), FREE).await?;
        ensure!(empty.done && empty.candidate.is_none() && empty.after == second.after);
        ensure!(
            next(&tx, &installation, None, &|_, _| anyhow::bail!(
                "maintenance reservation refused"
            ))
            .await
            .is_err()
        );
        tx.rollback().await?;
        db.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn persisted_paid_and_native_work_hold_selection_and_recheck_before_write() -> Result<()>
    {
        let db = open().await?;
        let installation = "installation".try_into()?;
        let tenant = "tenant".try_into()?;
        for (partition, kind) in [
            (
                crate::storage::partition::learning_pending(&installation, &tenant)?,
                "owned_judge_pending",
            ),
            (
                crate::storage::partition::native_experiment_pending(&installation, &tenant)?,
                "native_experiment_unit_pending_v1",
            ),
        ] {
            let tx = db.begin().await?;
            let original = candidate("installation", "tenant", "terminal", "creator")?;
            store::insert(&tx, &original).await?;
            let selected = next(&tx, &installation, None, FREE).await?;
            let prepared = Prepared::new(&selected.candidate.context("terminal candidate")?, FREE)?;
            // A pending participant appears after preparation. Existence alone
            // holds the receipt; this path must not decode arbitrary job bodies.
            tx.execute(sqlite("INSERT INTO checkpoint_evolution (record_id,scope_id,kind,session_key,revision,body,installation_id,tenant_id,creator_principal_id) VALUES ('pending',?,?,NULL,0,'opaque work','installation','tenant','creator')", [partition.into(), kind.into()])).await?;
            let held = next(&tx, &installation, None, FREE).await?;
            ensure!(held.candidate.is_none() && held.done && held.after == Some(key("terminal")?));
            ensure!(!super::super::archive_one(&tx, &prepared, FREE).await?);
            let marker = tx.query_one(sqlite("SELECT storage_kind FROM tenancy_execution_receipts WHERE execution_id='terminal'", [])).await?.context("inline marker")?;
            ensure!(marker.try_get::<i64>("", "storage_kind")? == 0);
            // A separate tenant remains eligible despite the pending owner.
            store::insert(
                &tx,
                &candidate("installation", "z-other", "other", "creator")?,
            )
            .await?;
            let other = next(&tx, &installation, held.after.as_ref(), FREE).await?;
            ensure!(
                other
                    .candidate
                    .context("other tenant candidate")?
                    .receipt
                    .tenant_id
                    .as_str()
                    == "z-other"
            );
            tx.execute_unprepared("DELETE FROM checkpoint_evolution WHERE record_id='pending'")
                .await?;
            ensure!(super::super::archive_one(&tx, &prepared, FREE).await?);
            tx.rollback().await?;
        }
        db.close().await?;
        Ok(())
    }

    #[test]
    fn prepared_estimates_include_sidecars_and_preserve_known_unclean_effects() -> Result<()> {
        use crate::tenancy::receipts::{auxiliary, capacity::ROW_OVERHEAD, usage_evidence};
        use bitrouter_ai::tenancy::{
            authority::{OwnerKind, ResourceKind},
            configuration::ResourceRef,
        };
        let credential = ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: "tenant".try_into()?,
            resource_kind: ResourceKind::Credential,
            resource_id: "credential".try_into()?,
            revision_id: "revision".try_into()?,
        };
        let original = candidate("installation", "tenant", "accounting", "creator")?
            .with_usage_evidence(usage_evidence::Snapshot::default())?;
        let fact = auxiliary::Fact {
            slot: crate::tenancy::oauth::storage::slot(
                &original.receipt.installation_id,
                &credential,
            )?,
            credential,
            generation: 0,
            work: uuid::Uuid::new_v4().to_string(),
            request_digest: None,
            http_status: None,
            outcome: auxiliary::Outcome::NotSent,
        };
        let original = original.with_auxiliary(vec![fact])?;
        ensure!(original.auxiliary_unclean() && !original.auxiliary_unknown());
        let prepared = Prepared::new(&original, FREE)?;
        ensure!(
            prepared.source_bytes() == original.bytes() + original.auxiliary_bytes() + ROW_OVERHEAD
        );
        ensure!(
            prepared.source_bytes().checked_sub(prepared.index_bytes())
                == Some(u64::try_from(prepared.relocated_bytes())? + 2 * ROW_OVERHEAD)
        );
        let expected_history = u64::try_from(prepared.stored_bytes())?
            + u64::try_from(original.private_json.len())?
            + u64::try_from(
                original
                    .auxiliary
                    .as_ref()
                    .context("auxiliary evidence")?
                    .json
                    .len(),
            )?
            + ROW_OVERHEAD;
        drop(original);
        ensure!(prepared.history_bytes() == expected_history);
        Ok(())
    }
}
