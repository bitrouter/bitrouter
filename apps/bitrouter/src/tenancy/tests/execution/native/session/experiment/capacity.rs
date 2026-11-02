//! The fixture must fund actual callbacks without bypassing cleanup admission.
use super::*;
use crate::{
    gateway::pipeline::native_work::NativeProviderWorkKind,
    storage::checkpoint::snapshot::{ResourceConstraint, RunStatus},
};
use bitrouter_ai::core::protocol::{CommitStatus, ErrorCode};

async fn campaign(undersized: bool) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, _) = fixture(directory.path(), &url, 1_000_000, 4).await?;
        let mut session = open(&fixture, &headers, &url, directory.path(), "capacity").await?;
        let checked: Result<()> = async {
            let mut input = task();
            if undersized {
                input
                    .limits
                    .as_mut()
                    .context("explicit run limits")?
                    .checkpoint_bytes = 128 * 1024;
            }
            let expected = input.limits.clone().context("explicit run limits")?;
            // Only the checkpoint profile differs. This is not another model
            // attempt, a longer execution window, or a relaxed wire/ACK bound.
            ensure!(expected.model_attempts == 1 && expected.active_seconds == 30);
            ensure!(expected.unacknowledged_bytes == 256 * 1024);
            let state = session
                .run(input, Arc::new(Approval), CancellationToken::new())
                .await?;
            drained(&fixture).await?;
            let run = state.run.as_ref().context("actual run")?;
            ensure!(
                run.limits == expected,
                "runtime widened the caller's limits"
            );
            let work: Vec<_> = state
                .agents
                .values()
                .filter_map(|agent| agent.turn.as_ref())
                .flat_map(|turn| &turn.steps)
                .flat_map(|step| &step.attempts)
                .flat_map(|attempt| &attempt.provider_work)
                .collect();
            if undersized {
                ensure!(run.status == RunStatus::Failed);
                ensure!(run.resource_constraint == Some(ResourceConstraint::CheckpointCapacity));
                let error = run
                    .resource_error
                    .as_ref()
                    .context("committed capacity refusal")?;
                ensure!(
                    error.code == ErrorCode::LimitExceeded
                        && error.commit_status == CommitStatus::Committed
                );
                ensure!(run.active_ms < expected.active_seconds * 1000);
                ensure!(run.model_attempts == 1);
                ensure!(
                    work.iter().any(|record| record.work.kind
                        == NativeProviderWorkKind::Authentication
                        && record
                            .report
                            .as_ref()
                            .is_some_and(|report| report.error_code.is_none())),
                    "capacity regression did not reach actual authentication"
                );
                ensure!(
                    work.iter()
                        .all(|record| record.work.kind != NativeProviderWorkKind::HttpDispatch),
                    "undersized cleanup profile admitted a physical dispatch"
                );
                ensure!(
                    server
                        .received_requests()
                        .await
                        .context("provider requests")?
                        .is_empty()
                );
            } else {
                ensure!(
                    run.status == RunStatus::Completed,
                    "bounded profile failed: {:?}",
                    run.resource_error
                );
                ensure!(run.resource_error.is_none() && run.resource_constraint.is_none());
                ensure!(
                    work.iter()
                        .filter(|record| record.work.kind == NativeProviderWorkKind::HttpDispatch)
                        .count()
                        == 1
                );
                // Also checks exact model/auth, durable original 4/2 usage,
                // completed assignment, publication and acknowledged step facts.
                ensure!(chain(&fixture, &server, &run.run_id).await? == ExperimentArm::Challenger);
                ensure!(
                    server
                        .received_requests()
                        .await
                        .context("provider requests")?
                        .len()
                        == 1
                );
            }
            Ok(())
        }
        .await;
        // Keep cleanup on the failure path too; a capacity refusal does not
        // authorize abandoning the native owner or its pending receipt work.
        let closed = tokio::time::timeout(TIMEOUT, session.close()).await;
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        closed??;
        stopped??;
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn original_small_checkpoint_refuses_before_http_dispatch() -> Result<()> {
    campaign(true).await
}

#[tokio::test]
async fn bounded_checkpoint_completes_with_the_full_routing_profile() -> Result<()> {
    campaign(false).await
}
