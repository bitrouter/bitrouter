//! Real server qualification of composite RR operations, isolated by namespace.
mod lifecycle;

use super::*;
use sea_orm::{IsolationLevel, TransactionTrait};

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL for a dedicated PostgreSQL or MySQL test database"]
async fn server_owned_eval_composites_share_rr_transaction_and_scoped_winner_reads() -> Result<()> {
    let url = std::env::var("BITROUTER_TEST_DATABASE_URL")
        .context("dedicated server database required")?;
    let parsed = url::Url::parse(&url)?;
    match parsed.scheme() {
        "postgres" | "postgresql" => ensure!(cfg!(feature = "db-postgres"), "enable db-postgres"),
        "mysql" => ensure!(cfg!(feature = "db-mysql"), "enable db-mysql"),
        _ => anyhow::bail!("dedicated PostgreSQL or MySQL database required"),
    }
    ensure!(
        !parsed.path().trim_matches('/').is_empty(),
        "explicit database name required"
    );
    tokio::time::timeout(Duration::from_secs(120), async {
        let mut db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        let (sender, queries) = std::sync::mpsc::channel();
        db.set_metric_callback(move |info| {
            let query = info
                .statement
                .sql
                .replace(['"', '\x60'], "")
                .to_ascii_lowercase();
            if sender.send(query).is_err() {
                tracing::debug!("owned SQL fixture receiver closed");
            }
        });
        let store = scoped(&db)?;
        let tx = db
            .begin_with_config(Some(IsolationLevel::RepeatableRead), None)
            .await?;
        let outcome = async {
            store
                .put_owned_in(&tx, &tests::subject()?, &|| Ok(()))
                .await?;
            let pass = result(EvalVerdict::Pass, "pass");
            let first = store.submit_owned_in(&tx, &pass, &|| Ok(())).await?;
            assert!(!first.duplicate);
            let replay = store.submit_owned_in(&tx, &pass, &|| Ok(())).await?;
            assert!(replay.duplicate);
            let snapshot = store
                .freeze_owned_in(&tx, "2026-10-25T00:00:00Z", &|| Ok(()))
                .await?;
            assert_eq!(snapshot.entries.len(), 1);
            let changed = result(EvalVerdict::Fail, "pass");
            assert!(matches!(
                store.submit_owned_in(&tx, &changed, &|| Ok(())).await,
                Err(OwnedError::Rejected(ErrorCode::InvalidRequest))
            ));
            // A confirmed content conflict must leave this transaction usable.
            let failure = store
                .submit_owned_in(&tx, &result(EvalVerdict::Fail, "fail"), &|| Ok(()))
                .await?;
            assert_eq!(failure.status, AdmissionStatus::Disputed);
            let frozen = store
                .snapshot_owned_in(&tx, &snapshot.evidence_root, &|| Ok(()))
                .await?;
            assert_eq!(frozen, snapshot);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        tx.rollback().await?;
        let remaining = store.list_subjects().await?;
        db.close().await?;
        outcome?;
        let queries = queries.try_iter().collect::<Vec<_>>();
        let reads = queries
            .iter()
            .filter(|sql| sql.starts_with("select ") && sql.contains("from eval_"));
        for sql in reads {
            let (_, predicate) = sql.split_once("where ").context("unscoped eval SELECT")?;
            assert!(
                predicate.contains("installation_id") && predicate.contains("tenant_id"),
                "unscoped eval SELECT: {sql}"
            );
        }
        let insert = queries
            .iter()
            .find(|sql| sql.starts_with("insert into eval_admission_events"))
            .context("admission insertion missing")?;
        if let Some((_, returning)) = insert.split_once(" returning ") {
            assert!(returning.contains("sequence") && !returning.contains("reason"));
        }
        assert!(remaining.is_empty());
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("owned server regression exceeded deadline")?
}
