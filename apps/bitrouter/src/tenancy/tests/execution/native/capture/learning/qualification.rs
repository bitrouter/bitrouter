//! Additional unrun qualification sources; no authority/recovery implementation.
use super::*;
use crate::tenancy::{
    capture::queue::{Reply, Request},
    transaction::{Disposition, Phase},
};
mod authority;
mod controller;
mod crash;
mod h_faults;
mod identity;
mod integrity;
mod process;
#[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
mod server;

async fn file_ready(path: &Path, subject: &str, judge: &str) -> Result<Ready> {
    let owner = crate::tenancy::ownership::acquire_path(path, true)?;
    let db = crate::db::connect_tenancy(&format!("sqlite://{}", path.display()), &owner).await?;
    let fixture = Fixture::with_database(db, OperationLimits::default(), &mut |_| {}).await?;
    let ready = Ready::from_fixture(
        fixture,
        Some(owner),
        subject,
        judge,
        true,
        LearningMode::Observe,
        2,
    )
    .await?;
    ensure!(
        std::fs::metadata(path)?.len() > 0,
        "physical fixture was never provisioned"
    );
    Ok(ready)
}
async fn frozen(
    binding: &Arc<crate::tenancy::capture::Binding>,
) -> Result<crate::acp_trajectory::checkpoint::owned::Summary> {
    let outcome = tokio::time::timeout(
        TIMEOUT,
        binding
            .enqueue(Request::Freeze(native().into()))
            .await
            .map_err(failure)?,
    )
    .await??;
    match outcome.result.map_err(failure)? {
        Reply::Frozen(Some(summary)) => Ok(summary),
        _ => anyhow::bail!("actual stopped checkpoint absent"),
    }
}
fn file_held(path: &Path) -> Result<()> {
    match crate::tenancy::ownership::acquire_path(path, false) {
        Err(error) => ensure!(
            matches!(
                error.downcast_ref::<std::fs::TryLockError>(),
                Some(std::fs::TryLockError::WouldBlock)
            ),
            "unexpected file ownership error: {error:#}"
        ),
        Ok(owner) => {
            drop(owner);
            anyhow::bail!("expected live file custody was released");
        }
    }
    Ok(())
}
