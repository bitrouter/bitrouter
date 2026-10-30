//! The production correlated consumer rejects its next obligation before egress.
use super::*;

#[tokio::test]
async fn long_identity_episode_reserves_terminal_closure_before_next_provider_send() -> Result<()> {
    let server = upstream(false, false).await;
    let policy_name = "p".repeat(128);
    let tier_name = "t".repeat(128);
    let mut document = document();
    let mut selected = document.policies.remove("selected").context("policy")?;
    let target = selected.tiers.remove("economy").context("target")?;
    selected.tiers.insert(tier_name.clone(), target);
    selected.default_tier = Some(tier_name.clone());
    let guard = selected.progress_guard.as_mut().context("guard")?;
    guard.escalation_tier = tier_name.clone();
    guard.protected_tiers = [tier_name].into();
    document.policies.insert(policy_name.clone(), selected);
    let (fixture, revision) = policy_fixture_named(
        &format!("{}/v1", server.uri()),
        LearningMode::Observe,
        document,
        true,
        Some(json!({"enabled":true})),
        &policy_name,
    )
    .await?;
    let headers = credential(&fixture, &revision).await?;
    let mut messages = vec![json!({"role":"user","content":PRIVATE_PROMPT})];
    let mut completed = 0usize;
    let mut rejected = false;
    for turn in 0..128 {
        let (status, bytes) = send(
            &fixture,
            &headers,
            json!({"model":MODEL,"messages":messages}),
        )
        .await?;
        // This is a sequential history-capacity test. Both accepted and
        // rejected attempts must release the one active execution slot before
        // the next probe can distinguish serialization capacity from admission.
        tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
        if !status.is_success() {
            ensure!(
                status == StatusCode::TOO_MANY_REQUESTS,
                "{status}: {}",
                String::from_utf8_lossy(&bytes)
            );
            let error: bitrouter_ai::tenancy::operation::Error = serde_json::from_slice(&bytes)?;
            ensure!(error.code == ErrorCode::CapacityExceeded);
            ensure!(
                error.dispatch_state == bitrouter_ai::tenancy::operation::DispatchState::NotStarted
            );
            rejected = true;
            break;
        }
        completed += 1;
        // Keep exact full history so every turn resolves the same retained partition.
        messages.push(json!({"role":"assistant","content":"observed"}));
        messages.push(json!({"role":"user","content":format!("continue-{turn}")}));
    }
    ensure!(
        rejected && completed > 1 && completed < 128,
        "capacity boundary after {completed} successful requests; rejected={rejected}"
    );
    ensure!(server.received_requests().await.context("requests")?.len() == completed);
    ensure!(count(&fixture.db, "trajectory_episodes", "1=1").await? == 1);
    ensure!(count(&fixture.db, "trajectory_requests", "status = 'started'").await? == 0);
    ensure!(count(&fixture.db, "trajectory_requests", "1=1").await? == i64::try_from(completed)?);
    ensure!(count(&fixture.db, "trajectory_outbox", "delivered_at IS NULL").await? == 0);
    for table in [
        "trajectory_outbox",
        "eval_subjects",
        "eval_results",
        "eval_admission_events",
    ] {
        ensure!(count(&fixture.db, table, "1=1").await? == i64::try_from(completed)?);
    }
    // A capacity rejection does not fence management or poison the next namespace.
    fixture.service.current().map_err(failure)?;
    fixture.issue("after-trajectory-capacity").await?;
    let (status, bytes) = send(
        &fixture,
        &headers,
        json!({"model":MODEL,"messages":[{"role":"user","content":"independent-root"}]}),
    )
    .await?;
    ensure!(
        status.is_success(),
        "{status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    fixture.service.pipelines.close_and_drain().await?;
    ensure!(server.received_requests().await.context("requests")?.len() == completed + 1);
    ensure!(count(&fixture.db, "trajectory_requests", "status = 'started'").await? == 0);
    fixture.service.shutdown().await?;
    Ok(())
}
