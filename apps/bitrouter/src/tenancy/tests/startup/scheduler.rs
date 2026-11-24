//! Explain the real scheduler and audit-key readers over retained unknown work.
use super::*;
use crate::tenancy::receipts::store::obligations;
use bitrouter_ai::tenancy::{
    execution::{Dispatch, ExecutionReceipt, Outcome, Provenance, ReceiptState, Usage},
    operation::DispatchState,
};

const ROWS: usize = 8 * obligations::VALIDATION_PAGE_ROWS + 1;

async fn seed(url: &str) -> Result<([u8; 32], InstallationId, chrono::DateTime<Utc>)> {
    let database = crate::db::connect_owned_tenancy(url).await?;
    database.migrate().await?;
    let db = database.database().clone();
    let service =
        Service::open_owned_profile(database, &Default::default(), &Default::default()).await?;
    let fixture = Fixture::with_service(db, service, &mut |_| {}).await?;
    fixture.service.shutdown().await?;
    let recorded = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let first = chrono::DateTime::parse_from_rfc3339(&recorded)?.with_timezone(&Utc);
    let tx = fixture.db.begin().await?;
    // Reverse insertion and a shared due timestamp make the complete raw
    // (due, tenant, execution) ordering observable, beyond the first page.
    for index in (0..ROWS).rev() {
        let candidate = receipts::Candidate::new(
            ExecutionReceipt {
                dispatch: Dispatch::Strict,
                installation_id: fixture.installation.clone(),
                tenant_id: fixture.tenant.clone(),
                principal_id: fixture.principal.clone(),
                execution_id: format!("retained-unknown-{index:04}").try_into()?,
                correlation_id: "scheduler-correlation".try_into()?,
                execution_revision: "scheduler-revision".try_into()?,
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
                recorded_at: recorded.clone().try_into()?,
            },
            receipts::Disposition {
                dispatch: DispatchState::Unknown,
                statuses: Vec::new(),
                auxiliary_digest: None,
            },
        )?;
        let staged = receipts::stage_terminal(&tx, &candidate).await?;
        ensure!(staged.inserted());
        drop(staged.reconcile(&|_, _| Ok(())).await?);
    }
    tx.commit().await?;
    ensure!(agreement(&fixture.db, &fixture.installation).await?[5] == u64::try_from(ROWS)?);
    let result = (
        fixture.wrapping_key,
        fixture.installation.clone(),
        first + chrono::Duration::minutes(15),
    );
    owned::dispose(fixture).await?;
    Ok(result)
}

pub(super) async fn exercise(url: &str) -> Result<()> {
    let (key, installation, due) = seed(url).await?;
    let queries = Arc::new(Queries::default());
    // Establish a key epoch once, then observe mature restarts over the same
    // unresolved set. No resource, route, or provider fixture is required.
    close(open_url(url, &queries, key).await?).await?;
    for _ in 0..2 {
        let service = open_url(url, &queries, key).await?;
        let checked: Result<()> = async {
            queries.require_bounded()?;
            queries.require_no_history_writes()?;
            let scheduler = queries.scheduler_statement()?;
            queries.require_indexed_retained_reads(&service.db).await?;

            let pages = Arc::new(Queries::default());
            let observed = pages.clone();
            let mut db = service.db.clone();
            db.set_metric_callback(move |info| observed.record(info));
            pages.start()?;
            let next = obligations::next_due(&db, &installation, &|_, _| Ok(())).await;
            let page = obligations::due_page(&db, &installation, due, &|_, _| Ok(())).await;
            let keys =
                obligations::sources::receipt_keys(&db, &installation, None, &|_, _| Ok(())).await;
            pages.stop();
            ensure!(next? == Some(due));
            let page = page?;
            let keys = keys?;
            ensure!(page.len() == obligations::VALIDATION_PAGE_ROWS && keys.len() == page.len());
            ensure!(keys.iter().zip(&page).all(|(key, row)| key == &row.key));
            pages.require_indexed_retained_reads(&db).await?;
            ensure!(
                keys.first().context("first key")?.execution.as_str() == "retained-unknown-0000"
            );
            let last = keys.last().context("final page key")?;
            pages.start()?;
            let continuation =
                obligations::sources::receipt_keys(&db, &installation, Some(last), &|_, _| Ok(()))
                    .await;
            pages.stop();
            let continuation = continuation?;
            ensure!(continuation.len() == obligations::VALIDATION_PAGE_ROWS);
            ensure!(
                continuation
                    .first()
                    .context("continuation key")?
                    .execution
                    .as_str()
                    == format!("retained-unknown-{:04}", obligations::VALIDATION_PAGE_ROWS)
            );
            pages.require_indexed_retained_reads(&db).await?;

            // Replay the observed query with only its ORDER BY qualification
            // removed, reproducing the guarded-alias regression. The plan guard
            // must refuse this whole-range sort even though the query has LIMIT 1.
            let mut alias_order = scheduler.clone();
            for column in ["due_seconds", "due_nanos", "tenant_id", "execution_id"] {
                alias_order.sql = alias_order.sql.replace(
                    &format!("\"tenancy_unknown_obligations\".\"{column}\""),
                    &format!("\"{column}\""),
                );
            }
            ensure!(
                alias_order.sql != scheduler.sql,
                "scheduler ORDER BY does not qualify raw table columns"
            );
            pages.start()?;
            let old = db.query_one(alias_order).await;
            pages.stop();
            ensure!(old?.is_some());
            let refused = pages
                .require_indexed_retained_reads(&db)
                .await
                .err()
                .context("guarded-alias scheduler sort was not rejected")?;
            ensure!(
                refused.to_string().contains("sorts its range"),
                "wrong plan refusal: {refused:#}"
            );
            ensure!(agreement(&service.db, &installation).await?[5] == u64::try_from(ROWS)?);
            ensure!(service.current().is_ok() && service.admission.receipts_ready());
            Ok(())
        }
        .await;
        let stopped = close(service).await;
        checked.and(stopped)?;
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_owned_scheduler_and_receipt_key_pages_never_sort_retained_unknowns() -> Result<()> {
    let home = tempfile::tempdir()?;
    exercise(&format!(
        "sqlite://{}",
        home.path().join("scheduler.sqlite").display()
    ))
    .await
}
