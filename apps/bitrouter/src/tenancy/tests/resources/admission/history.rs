use super::*;
use crate::tenancy::{actor::AuthoritySubject, resources::lifecycle, store::startup};
use sea_orm::TransactionTrait;

async fn count(fixture: &Fixture, table: &str) -> Result<i64> {
    fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!("SELECT COUNT(*) AS n FROM {table}"),
        ))
        .await?
        .context("count")?
        .try_get("", "n")
        .map_err(Into::into)
}

#[tokio::test]
async fn admission_history_4095_4096_4097_upgrade_user_write_replay_and_failed_commit() -> Result<()>
{
    let observed = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recording = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut db = crate::db::connect("sqlite::memory:").await?;
    let queries = observed.clone();
    let enabled = recording.clone();
    db.set_metric_callback(move |info| {
        if enabled.load(std::sync::atomic::Ordering::Relaxed)
            && let Ok(mut queries) = queries.lock()
        {
            queries.push(info.statement.sql.clone());
        }
    });
    let mut fixture = Fixture::with_database(db, &mut |_| {}).await?;
    restart(&mut fixture).await?;
    fixture.service.shutdown().await?;
    let tx = fixture.db.begin().await?;
    let (head, historical_route) = fixture
        .service
        .resources
        .seed_admission_history_for_test(&tx, 3647, 448)
        .await?;
    // Recreate the actual predecessor schema and ledger, preserving all sealed
    // history and managed lifecycle rows. No environment data is used.
    for sql in [
        "DROP TABLE tenancy_admission_index",
        "DROP TABLE tenancy_admission_backfill",
        "DROP INDEX tenancy_resource_head_lookup",
        "DELETE FROM seaql_migrations WHERE version IN ('m20240101_000049_admission_index','m20240101_000050_admission_index_fence')",
        "UPDATE tenancy_installation SET schema_version=5",
    ] {
        tx.execute_unprepared(sql).await?;
    }
    tx.commit().await?;
    ensure!(count(&fixture, "tenancy_resource_revisions").await? == 4096); // 4095 facts + credential
    crate::db::run_migrations(&fixture.db).await?;
    // The exact predecessor header writer cannot commit into format six.
    let mut old = startup::read_header_in(&fixture.db, 4096)
        .await?
        .context("header")?;
    ensure!(old.format == startup::Format::V6);
    old.format = startup::Format::V5;
    let tx = fixture.db.begin().await?;
    ensure!(startup::write_header_in(&tx, &old).await.is_err());
    tx.rollback().await?;
    restart(&mut fixture).await?;
    ensure!(count(&fixture, "tenancy_admission_index").await? == 2);

    let mut pinned = head.clone();
    pinned.revision_id = "v1".try_into()?;
    let identity = fixture
        .service
        .authority_read(AuthoritySubject::Profiles {
            caller: fixture.operator.clone(),
            references: vec![pinned.clone(), historical_route.clone()],
        })
        .await
        .map_err(failure)?;
    let original = credential(&fixture, "fact-4096")?;
    let created = put(&fixture, fixture.operator.clone(), original.clone()).await?;
    let created_ref = reference(&original, &created)?;
    ensure!(count(&fixture, "tenancy_resource_revisions").await? == 4097);
    let mut next = original;
    next.operation_id = "fact-4097".try_into()?;
    next.expected_revision = Some(created_ref.revision_id.clone());
    next.definition = Definition::Credential {
        secret: uuid::Uuid::new_v4().to_string(),
    };
    let revisions = count(&fixture, "tenancy_resource_revisions").await?;
    fixture.db.execute_unprepared("CREATE TRIGGER refuse_admission_operation BEFORE INSERT ON tenancy_operations WHEN NEW.operation_id='fact-4097' BEGIN SELECT RAISE(ABORT,'injected commit rollback'); END").await?;
    ensure!(
        put(&fixture, fixture.operator.clone(), next.clone())
            .await
            .is_err()
    );
    ensure!(count(&fixture, "tenancy_resource_revisions").await? == revisions);
    ensure!(
        lifecycle::head(&fixture.db, &fixture.installation, &created_ref.key())
            .await?
            .context("head")?
            .reference
            == created_ref
    );
    fixture
        .db
        .execute_unprepared("DROP TRIGGER refuse_admission_operation")
        .await?;
    drop(identity);
    restart(&mut fixture).await?;
    let identity = fixture
        .service
        .authority_read(AuthoritySubject::Profiles {
            caller: fixture.operator.clone(),
            references: vec![pinned.clone(), historical_route.clone()],
        })
        .await
        .map_err(failure)?;
    recording.store(true, std::sync::atomic::Ordering::Relaxed);
    let applied = put(&fixture, fixture.operator.clone(), next.clone()).await?;
    let replay = put(&fixture, fixture.operator.clone(), next).await?;
    recording.store(false, std::sync::atomic::Ordering::Relaxed);
    ensure!(
        applied.resource_revision == replay.resource_revision
            && applied.authority_revision == replay.authority_revision
    );
    ensure!(count(&fixture, "tenancy_resource_revisions").await? == revisions + 1);
    ensure!(count(&fixture, "tenancy_admission_index").await? == 2);
    fixture
        .service
        .authority
        .read()
        .map_err(|code| failure(code.into()))?
        .check(&identity.lease, |view| {
            let state = view.facts();
            if !state.resource_admission.protected(&historical_route)
                || state
                    .resource_admission
                    .current_profile(&pinned)
                    .map(|(r, _)| r.revision_id)
                    != Some(head.revision_id.clone())
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            Ok(())
        })
        .map_err(|code| failure(code.into()))?;
    {
        let queries = observed
            .lock()
            .map_err(|_| anyhow::anyhow!("SQL observation poisoned"))?;
        ensure!(
            queries.len() < 100,
            "ordinary publish/replay queried history: {} statements",
            queries.len()
        );
        ensure!(
            !queries
                .iter()
                .any(|sql| sql.contains("ORDER BY") && sql.contains("tenancy_resource_revisions"))
        );
    }
    drop(identity);
    restart(&mut fixture).await?;
    let identity = fixture
        .service
        .authority_read(AuthoritySubject::Profiles {
            caller: fixture.operator.clone(),
            references: vec![pinned.clone(), historical_route.clone()],
        })
        .await
        .map_err(failure)?;
    fixture
        .service
        .authority
        .read()
        .map_err(|code| failure(code.into()))?
        .check(&identity.lease, |view| {
            view.facts()
                .resource_admission
                .configuration(std::slice::from_ref(&historical_route))
        })
        .map_err(|code| failure(code.into()))?;
    drop(identity);
    fixture.service.shutdown().await?;
    Ok(())
}
