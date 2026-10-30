use super::*;
use crate::harness::trajectory::owned::entities::records as rows;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

#[tokio::test]
async fn actual_cohort_old_root_cannot_replace_a_removed_unit_proof() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, _) = fixture(directory.path(), &url, 1_000_000, 4).await?;
        let checked = async {
            let mut session =
                open(&fixture, &headers, &url, directory.path(), "removed-proof").await?;
            let result = async {
                let state = session
                    .run(task(), Arc::new(Approval), CancellationToken::new())
                    .await?;
                let run = state.run.as_ref().context("completed run")?;
                ensure!(run.status == crate::storage::checkpoint::snapshot::RunStatus::Completed);
                ensure!(chain(&fixture, &server, &run.run_id).await? == ExperimentArm::Challenger);
                let frozen = freeze(&fixture, &headers).await?;
                let before = session
                    .experiment_cohort(MODEL, &frozen.evidence_root)
                    .await?;
                ensure!(before.challenger.observed == 1 && before.challenger.conclusive == 0);
                // Only a negative corruption is injected. The positive unit,
                // receipt, Eval and ACK above all came from the real owner.
                let removed = rows::Entity::delete_many()
                    .filter(rows::Column::InstallationId.eq(fixture.installation.as_str()))
                    .filter(rows::Column::TenantId.eq(fixture.tenant.as_str()))
                    .filter(rows::Column::Kind.eq("native_experiment_unit_v1"))
                    .exec(&fixture.db)
                    .await?;
                ensure!(removed.rows_affected == 1);
                ensure!(
                    session
                        .experiment_cohort(MODEL, &frozen.evidence_root)
                        .await
                        .is_err()
                );
                ensure!(fixture.service.current().is_ok());
                ensure!(server.received_requests().await.context("requests")?.len() == 1);
                Ok::<(), anyhow::Error>(())
            }
            .await;
            let closed = tokio::time::timeout(TIMEOUT, session.close()).await;
            result?;
            closed??;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        stopped??;
        Ok(())
    })
    .await?
}
