use super::*;

#[tokio::test]
async fn duplicate_checkpoint_during_the_live_judge_returns_the_original_job_without_another_send()
-> Result<()> {
    let provider = Provider::start(false).await?;
    let mut judge = Judge::new(true).await?;
    let ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
    let port = ready.port().await?;
    let original = port.clone();
    let first = AbortOnDropHandle::new(tokio::spawn(
        async move { original.checkpoint(native()).await },
    ));
    let checked = async {
        judge.receive().await?;
        let accepted = bodies(&ready.fixture, "owned_judge_job").await?;
        ensure!(accepted.len() == 1 && accepted[0]["status"] == "accepted");
        let duplicate = tokio::time::timeout(TIMEOUT, port.checkpoint(native())).await??;
        ensure!(duplicate.is_some());
        ensure!(!first.is_finished() && judge.calls.load(Ordering::SeqCst) == 1);
        ensure!(bodies(&ready.fixture, "owned_judge_job").await? == accepted);
        ensure!(bodies(&ready.fixture, "owned_judge_budget").await?[0]["accepted"] == 1);
        judge.release.cancel();
        ensure!(tokio::time::timeout(TIMEOUT, first).await??? == duplicate);
        drained(&ready).await?;
        ensure!(judge.calls.load(Ordering::SeqCst) == 1);
        ensure!(count(&ready.fixture, "acp_assessment_revisions").await? == 1);
        ensure!(
            bodies(&ready.fixture, "owned_judge_pending")
                .await?
                .is_empty()
        );
        Ok(())
    }
    .await;
    cleanup(&ready, &[port], judge, provider, checked).await
}

#[tokio::test]
async fn paid_child_fails_fast_when_the_parent_owns_the_last_slot_and_can_run_after_release()
-> Result<()> {
    let provider = Provider::start(false).await?;
    let judge = Judge::new(false).await?;
    let mut ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
    ready.revision = ready
        .configuration_with_active("single-slot", LearningMode::Observe, 1, 1)
        .await?;
    let port = ready.port().await?;
    let checked = async {
        let submission = ready
            .fixture
            .service
            .submission(&ready.headers()?)
            .map_err(failure)?;
        let parent = ready
            .fixture
            .service
            .admit_execution(
                submission,
                ready.subject.resource_id.as_str(),
                &ApiProtocol::ChatCompletions,
            )
            .await
            .map_err(failure)?;
        let denied = tokio::time::timeout(Duration::from_secs(1), port.checkpoint(native())).await;
        ensure!(
            matches!(denied, Ok(Err(_))),
            "nested paid admission queued behind its parent"
        );
        ensure!(
            bodies(&ready.fixture, "owned_judge_job").await?.is_empty()
                && judge.calls.load(Ordering::SeqCst) == 0
        );
        drop(parent);
        drained(&ready).await?;
        ensure!(port.checkpoint(native()).await?.is_some());
        drained(&ready).await?;
        ensure!(
            judge.calls.load(Ordering::SeqCst) == 1
                && bodies(&ready.fixture, "owned_judge_budget").await?[0]["accepted"] == 1
        );
        Ok(())
    }
    .await;
    cleanup(&ready, &[port], judge, provider, checked).await
}

#[tokio::test]
async fn bootstrap_refuses_a_durable_pending_job_instead_of_reconstructing_send_authority()
-> Result<()> {
    let provider = Provider::start(false).await?;
    let mut judge = Judge::new(true).await?;
    let ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
    let port = ready.port().await?;
    let owned = port.clone();
    let task = AbortOnDropHandle::new(tokio::spawn(
        async move { owned.checkpoint(native()).await },
    ));
    let checked = async {
        judge.receive().await?;
        ensure!(bodies(&ready.fixture, "owned_judge_pending").await?.len() == 1);
        // This in-memory bootstrap refusal does not claim process-crash
        // recovery. The original Service still owns the actual provider.
        let initial = store::load(&ready.fixture.db, Utc::now()).await?;
        let attempt = Service::start_prepared(
            ready.fixture.db.clone(),
            None,
            OperationLimits::default(),
            initial,
            Arc::new(Resources::with_key(
                ready.fixture.installation.clone(),
                [29; 32],
            )?),
        )
        .await;
        if let Ok(service) = attempt {
            service.shutdown().await?;
            anyhow::bail!("pending row minted fresh readiness");
        }
        ensure!(judge.calls.load(Ordering::SeqCst) == 1 && ready.fixture.service.current().is_ok());
        judge.release.cancel();
        tokio::time::timeout(TIMEOUT, task).await???;
        drained(&ready).await?;
        ensure!(
            bodies(&ready.fixture, "owned_judge_pending")
                .await?
                .is_empty()
        );
        Ok(())
    }
    .await;
    cleanup(&ready, &[port], judge, provider, checked).await
}

#[tokio::test]
async fn lost_checkpoint_waiter_does_not_refund_acceptance_or_release_the_live_provider()
-> Result<()> {
    let provider = Provider::start(false).await?;
    let mut judge = Judge::new(true).await?;
    let ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
    let port = ready.port().await?;
    let owned = port.clone();
    let task = AbortOnDropHandle::new(tokio::spawn(
        async move { owned.checkpoint(native()).await },
    ));
    let checked = async {
        judge.receive().await?;
        let jobs = bodies(&ready.fixture, "owned_judge_job").await?;
        ensure!(jobs.len() == 1 && jobs[0]["status"] == "accepted");
        ensure!(bodies(&ready.fixture, "owned_judge_pending").await?.len() == 1);
        ensure!(
            ready
                .fixture
                .service
                .admission
                .wait_drained()
                .now_or_never()
                .is_none()
        );
        task.abort();
        ensure!(task.await.err().is_some_and(|error| error.is_cancelled()));
        // The provider/receipt may settle through cancellation; only their real
        // terminal ACK can release capacity. No timeout is asserted clean.
        judge.release.cancel();
        drained(&ready).await?;
        let terminal = bodies(&ready.fixture, "owned_judge_job").await?;
        ensure!(terminal.len() == 1 && terminal[0]["status"] != "accepted");
        ensure!(
            bodies(&ready.fixture, "owned_judge_pending")
                .await?
                .is_empty()
        );
        ensure!(bodies(&ready.fixture, "owned_judge_budget").await?[0]["accepted"] == 1);
        let receipt = receipt(
            &ready,
            terminal[0]["execution"].as_str().context("execution")?,
        )
        .await?;
        ensure!(
            receipt["attempts"]
                .as_array()
                .context("actual attempts")?
                .len()
                == 1
        );
        ensure!(port.checkpoint(native()).await?.is_some());
        drained(&ready).await?;
        ensure!(
            judge.calls.load(Ordering::SeqCst) == 1,
            "uncertain result was replayed"
        );
        Ok(())
    }
    .await;
    cleanup(&ready, &[port], judge, provider, checked).await
}

#[tokio::test]
async fn shutdown_waits_for_an_admitted_paid_execution_and_its_actor_completion() -> Result<()> {
    let provider = Provider::start(false).await?;
    let mut judge = Judge::new(true).await?;
    let ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
    let port = ready.port().await?;
    let owned = port.clone();
    let task = AbortOnDropHandle::new(tokio::spawn(
        async move { owned.checkpoint(native()).await },
    ));
    let checked = async {
        judge.receive().await?;
        // This direct-port fixture closes its capture producer explicitly; the
        // paid execution remains registered in the real Service Registry.
        port.finish().await?;
        let stopped = ready.fixture.service.host_stop();
        let shutdown = ready.fixture.service.shutdown();
        tokio::pin!(shutdown);
        tokio::select! {
            result = &mut shutdown => { result?; anyhow::bail!("shutdown released an active paid owner"); }
            () = stopped.cancelled() => {}
        }
        ensure!(shutdown.as_mut().now_or_never().is_none(), "held model owner was reported drained");
        judge.release.cancel();
        let _checkpoint = tokio::time::timeout(TIMEOUT, task).await??;
        tokio::time::timeout(TIMEOUT, shutdown).await??;
        let jobs = bodies(&ready.fixture, "owned_judge_job").await?;
        ensure!(jobs.len() == 1 && jobs[0]["status"] != "accepted");
        ensure!(bodies(&ready.fixture, "owned_judge_pending").await?.is_empty());
        ensure!(count(&ready.fixture, "acp_effective_assessments").await? == 0);
        receipt(&ready, jobs[0]["execution"].as_str().context("execution")?).await?;
        Ok(())
    }.await;
    cleanup(&ready, &[port], judge, provider, checked).await
}
