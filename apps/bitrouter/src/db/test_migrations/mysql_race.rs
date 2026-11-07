//! Pause only the test migrator between the runner's count and real rediscovery.
use std::{future::Future, time::Duration};

use sea_orm::{DatabaseConnection, DbErr};
use sea_orm_migration::{IntoSchemaManagerConnection, MigrationTrait, MigratorTrait};
use tokio::{sync::Notify, task::JoinHandle};

use super::super::migration::Migrator;

static ENTERED: Notify = Notify::const_new();
static RESUME: Notify = Notify::const_new();
struct Paused;

#[async_trait::async_trait]
impl MigratorTrait for Paused {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        Migrator::migrations()
    }

    async fn up<'c, C>(db: C, steps: Option<u32>) -> Result<(), DbErr>
    where
        C: IntoSchemaManagerConnection<'c>,
    {
        if steps == Some(1) {
            ENTERED.notify_one();
            tokio::time::timeout(Duration::from_secs(10), RESUME.notified())
                .await
                .map_err(|_| DbErr::Custom("paused migration fixture deadline".into()))?;
        }
        // This invokes the actual framework rediscovery after the stale count.
        // https://github.com/SeaQL/sea-orm/blob/1.1.20/sea-orm-migration/src/migrator.rs
        Migrator::up(db, steps).await
    }
}

struct Pending(JoinHandle<crate::gateway::error::Result<()>>);
impl Drop for Pending {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) async fn stale_prefix(
    db: &DatabaseConnection,
    advance: impl Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<crate::gateway::error::Result<()>> {
    let db = db.clone();
    let mut pending = Pending(tokio::spawn(async move {
        super::super::run_migrations_with::<Paused>(&db).await
    }));
    let result = async {
        tokio::time::timeout(Duration::from_secs(10), ENTERED.notified()).await?;
        advance.await?;
        RESUME.notify_one();
        Ok(tokio::time::timeout(Duration::from_secs(10), &mut pending.0).await??)
    }
    .await;
    if !pending.0.is_finished() {
        pending.0.abort();
        if let Err(error) = (&mut pending.0).await {
            anyhow::ensure!(
                error.is_cancelled(),
                "paused migration join failed: {error}"
            );
        }
    }
    result
}
