//! A retained local SQL owner performs the one acknowledged legacy transition.
use crate::{
    offline::{ValidationBudget, engine::Connections},
    tenancy::{
        audit::upgrade::{self, Outcome, Request},
        ownership,
    },
};
use anyhow::{Context, Result, ensure};
use futures::FutureExt;
use std::{panic::AssertUnwindSafe, path::Path};
use tokio_util::sync::CancellationToken;

impl crate::output::CliReport for Outcome {
    fn render(&self, output: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        output.line(&format!(
            "Legacy count fence upgrade recorded for installation {}: authority {} → {}, format 2.",
            self.installation_id, self.old_authority_revision, self.new_authority_revision
        ))?;
        output.line("This is the recorded operation result. Normal startup must validate current custody before serving.")
    }
}

pub(in crate::offline) async fn run(
    database: &Path,
    request: Request,
    budget: ValidationBudget,
) -> Result<Outcome> {
    ensure!(
        cfg!(unix),
        "capacity upgrade requires local Unix SQLite ownership"
    );
    super::plan::absolute(database)?;
    let metadata = std::fs::symlink_metadata(database)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "capacity upgrade requires an existing regular database file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            metadata.nlink() == 1 && metadata.permissions().mode() & 0o777 == 0o600,
            "capacity upgrade database must be private and unaliased"
        );
    }
    ensure!(
        budget.rows > 0 && budget.bytes > 0 && budget.bytes <= isize::MAX as usize,
        "invalid validation working budget"
    );
    let lease = ownership::acquire_path(database, false)?;
    let path = lease.path.clone();
    let cancel = CancellationToken::new();
    let _abandoned = cancel.clone().drop_guard();
    let (send, wait) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut connections = Connections::new(cancel, Some(lease));
        let result = AssertUnwindSafe(async {
            connections.validation_budget().await?;
            connections.open_destination(&path).await?;
            connections.uncertain = true;
            sea_orm::sqlx::query("BEGIN IMMEDIATE")
                .execute(
                    connections
                        .source
                        .as_mut()
                        .context("upgrade connection missing")?,
                )
                .await?;
            let owner = connections
                .owner
                .as_ref()
                .context("upgrade owner missing")?;
            let result = upgrade::apply_in(
                connections
                    .source
                    .as_mut()
                    .context("upgrade connection missing")?,
                &request,
                budget,
                || crate::tenancy::storage::exclusive_write(owner, 16 * 1024),
            )
            .await;
            let result = result.and_then(|outcome| {
                connections.check()?;
                Ok(outcome)
            });
            // Cleanup/COMMIT futures are never selected away by cancellation.
            connections.destination_cleanup_budget().await?;
            let sql = if result.is_ok() { "COMMIT" } else { "ROLLBACK" };
            sea_orm::sqlx::query(sql)
                .execute(
                    connections
                        .source
                        .as_mut()
                        .context("upgrade connection missing")?,
                )
                .await?;
            connections.uncertain = false;
            result
        })
        .catch_unwind()
        .await;
        let result = match result {
            Ok(result) => result,
            Err(_) => {
                connections.uncertain = true;
                Err(anyhow::anyhow!("capacity upgrade owner unwound"))
            }
        };
        let result = match connections.close().await {
            Ok(()) => result,
            Err(error) => Err(error),
        };
        if send.send(result).is_err() {
            tracing::debug!("capacity upgrade observer abandoned after cleanup");
        }
    });
    wait.await.context("capacity upgrade owner disappeared")?
}
