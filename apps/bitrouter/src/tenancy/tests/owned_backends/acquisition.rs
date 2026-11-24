//! Cancel an actual idle checkout before any Service or monitor can borrow it.
use super::*;
use sea_orm::{DatabaseBackend, DatabaseConnection, Statement};
use std::task::Poll;

async fn pid(db: &DatabaseConnection) -> Result<i32> {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        "SELECT pg_backend_pid() AS pid",
    ))
    .await?
    .context("owned PostgreSQL session")?
    .try_get("", "pid")
    .map_err(Into::into)
}

async fn lock_retained(admin: &DatabaseConnection, expected: i32) -> Result<()> {
    let row = admin
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT count(*)::bigint AS locks FROM pg_catalog.pg_locks WHERE pid=$1 AND locktype='advisory' AND mode='ExclusiveLock' AND granted",
            [expected.into()],
        ))
        .await?
        .context("owned PostgreSQL advisory lock")?;
    ensure!(
        row.try_get::<i64>("", "locks")? == 1,
        "idle acquisition cancellation released the original advisory lock"
    );
    Ok(())
}

pub(super) async fn exercise() -> Result<()> {
    let isolated = crate::db::migration::server_fixture::OwnedServer::new(
        DatabaseBackend::Postgres,
        "BITROUTER_TEST_MIGRATION_DATABASE_URL",
    )
    .await?;
    let outcome: Result<()> = async {
        let owned = crate::db::connect_owned_tenancy(&isolated.url).await?;
        owned.migrate().await?;
        let owner = owned.owner();
        let db = owned.database().clone();
        let pool = db.get_postgres_connection_pool();
        let mut guard = pool.acquire().await?;
        let expected: i32 = sea_orm::sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *guard)
            .await?;
        // Await the real return, including SQLx's release ping. No actor,
        // ownership monitor or fixture callback can race this idle checkout.
        guard.return_to_pool().await;
        ensure!(pool.size() == 1 && pool.num_idle() == 1);
        let cancelled = {
            let acquire = pool.acquire();
            tokio::pin!(acquire);
            match futures::poll!(acquire.as_mut()) {
                Poll::Ready(Ok(mut connection)) => {
                    connection.return_to_pool().await;
                    false
                }
                Poll::Ready(Err(error)) => return Err(error.into()),
                Poll::Pending => {
                    ensure!(
                        pool.num_idle() == 0,
                        "checkout suspended before removing the sole idle session"
                    );
                    true
                }
            }
            // A pending acquisition is cancelled by dropping its actual future,
            // matching optional preemption. With the old checkout ping, SQLx
            // has removed the session but has not returned a PoolConnection.
        };
        let actual = tokio::time::timeout(crate::tenancy::MANAGEMENT_TIMEOUT, pid(&db))
            .await
            .context("original owner unavailable after idle acquisition cancellation")??;
        ensure!(
            actual == expected && !owner.lost().is_cancelled(),
            "idle checkout replaced or fenced the owner: expected={expected}, actual={actual}, cancelled={cancelled}"
        );
        lock_retained(&isolated.database, expected).await?;

        // Exercise the real actor, authentication, final send admission and
        // protected receipt getter on this SAME connector after the checkout.
        let service =
            Service::open_owned_profile(owned, &Default::default(), &Default::default()).await?;
        let fixture = Fixture::with_service(db.clone(), service, &mut |_| {}).await?;
        let issued = fixture.issue("after-idle-checkout").await?;
        fixture
            .service
            .authenticate_live(issued.secret.as_deref().context("credential secret")?)
            .await
            .map_err(failure)?;
        let (fixture, _) =
            super::super::execution::receipts::owned_backend_dispatch(fixture, owner.clone())
                .await?;
        fixture.service.shutdown().await?;
        ensure!(pid(&db).await? == expected && !owner.lost().is_cancelled());
        lock_retained(&isolated.database, expected).await?;
        drop((fixture, owner));
        db.close().await?;
        Ok(())
    }
    .await;
    isolated.finish(outcome).await
}
