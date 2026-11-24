//! One physical PostgreSQL session owns all tenancy transactions.
use crate::tenancy::ownership::Lease;
use anyhow::Result;
use sea_orm::sqlx::{ConnectOptions as _, Postgres, postgres::PgConnectOptions};
use sea_orm::{ConnectOptions, DatabaseConnection};

// Session locks are scoped to the current database, including across schemas.
// https://www.postgresql.org/docs/current/explicit-locking.html#ADVISORY-LOCKS
const OWNER_LOCK: i64 = 0x4252_5445_4e41_4e54;

pub(super) async fn connect(
    url: &str,
    mut options: ConnectOptions,
    owner: Option<&Lease>,
) -> Result<DatabaseConnection> {
    anyhow::ensure!(
        owner.is_none_or(|owner| owner.is_postgres() && owner.exclusive()),
        "PostgreSQL ownership acquisition mismatch"
    );
    let pg = url.parse::<PgConnectOptions>()?.disable_statement_logging();
    let lease = owner.cloned();
    if owner.is_some() {
        options.max_connections(1).min_connections(0);
    }
    let mut pool_options = options.sqlx_pool_options::<Postgres>();
    if owner.is_some() {
        // Cancelling acquire during SQLx's idle ping drops that connection.
        // Handing the idle owner session to its caller must not suspend;
        // after_connect runs only for new sessions, which cannot replace it.
        pool_options = pool_options
            .max_lifetime(None)
            .idle_timeout(None)
            .test_before_acquire(false);
    }
    let pool = pool_options
        .after_connect(move |connection, _| {
            let lease = lease.clone();
            Box::pin(async move {
                let Some(lease) = lease else {
                    let locked: bool =
                        sea_orm::sqlx::query_scalar("SELECT pg_try_advisory_lock_shared($1)")
                            .bind(OWNER_LOCK)
                            .fetch_one(&mut *connection)
                            .await?;
                    if !locked {
                        return Err(sea_orm::sqlx::Error::Protocol(
                            "a tenancy process owns this database".into(),
                        ));
                    }
                    return Ok(());
                };
                // A lost session must never be replaced, even if a second owner
                // has not yet acquired its lock. Every write uses this pool.
                if !lease.connect_postgres_once() {
                    lease.lost().cancel();
                    return Err(sea_orm::sqlx::Error::Protocol(
                        "tenancy owner session lost".into(),
                    ));
                }
                let locked: bool = sea_orm::sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                    .bind(OWNER_LOCK)
                    .fetch_one(&mut *connection)
                    .await?;
                if !locked {
                    lease.lost().cancel();
                    return Err(sea_orm::sqlx::Error::Protocol(
                        "another process owns this tenancy database".into(),
                    ));
                }
                Ok(())
            })
        })
        .connect_with(pg)
        .await?;
    Ok(DatabaseConnection::from(pool))
}
