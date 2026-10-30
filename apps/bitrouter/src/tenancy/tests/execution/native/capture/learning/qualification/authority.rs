use super::*;

#[tokio::test(flavor = "current_thread")]
async fn paid_prepare_expiring_behind_a_real_receipt_does_not_begin_or_leak_capacity() -> Result<()>
{
    let provider = Provider::start(false).await?;
    let judge = Judge::new(false).await?;
    let ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
    let (binding, port) = ready.port_pair().await?;
    let summary = frozen(&binding).await?;
    let checked = async {
        let service = &ready.fixture.service;
        let submission = service.submission(&ready.headers()?).map_err(failure)?;
        let parent = service
            .admit_execution(
                submission,
                ready.subject.resource_id.as_str(),
                &ApiProtocol::ChatCompletions,
            )
            .await
            .map_err(failure)?;
        // A genuine admitted no-send receipt owns a 60s submission/30s drain
        // window. Its COMMIT can outlive Prepare's unchanged5s deadline without
        // expiring another management transaction or altering any limit.
        let proof = &service.cleanup;
        let gate = proof.hold(Phase::Committing);
        drop(parent);
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        let counts = proof.sql_counts_for_test();
        let reads = service.reads.available_permits();
        let mut prepare = Box::pin(crate::tenancy::learning::checkpoint(
            binding,
            native(),
            &summary,
        ));
        let observed: Result<()> = async {
            ensure!(futures::poll!(&mut prepare).is_pending());
            ensure!(service.reads.available_permits() == reads - 1);
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(6)).await;
            tokio::time::resume();
            ensure!(tokio::time::timeout(TIMEOUT, &mut prepare).await?.is_err());
            ensure!(proof.disposition() == Disposition::Pending);
            ensure!(service.reads.available_permits() == reads - 1);
            Ok(())
        }
        .await;
        drop(prepare);
        proof.release_gate();
        tokio::time::timeout(TIMEOUT, async {
            while service.reads.available_permits() != reads {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        drained(&ready).await?;
        observed?;
        ensure!(
            proof.sql_counts_for_test() == counts,
            "expired Prepare began SQL"
        );
        ensure!(bodies(&ready.fixture, "owned_judge_job").await?.is_empty());
        ensure!(judge.calls.load(Ordering::SeqCst) == 0 && provider.calls() == 0);
        ensure!(port.checkpoint(native()).await?.is_some());
        drained(&ready).await?;
        ensure!(judge.calls.load(Ordering::SeqCst) == 1);
        Ok(())
    }
    .await;
    ready.fixture.service.cleanup.release_gate();
    cleanup(&ready, &[port], judge, provider, checked).await
}

#[tokio::test]
async fn evaluator_final_send_rechecks_real_authority_after_authentication_callback_wait()
-> Result<()> {
    for change in ["revoke", "scope"] {
        let provider = Provider::start(false).await?;
        let judge = Judge::new(false).await?;
        let ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
        let (binding, port) = ready.port_pair().await?;
        // This focused actual evaluator admission/send test does not assert a
        // durable logical-job acceptance. It isolates the private binding's
        // post-auth await gate without adding a production control injection.
        let execution = binding
            .authority()
            .admit_assessment(
                crate::tenancy::learning::source::Binding::Capture(binding.clone()),
                &ready.judge,
            )
            .await
            .map_err(failure)?;
        let id = execution.id().to_string();
        let gate = Arc::new(Gate::default());
        let control = Arc::new(Control {
            hold: Some((Hold::AuthenticationOutcome, gate.clone())),
            ..Default::default()
        });
        let mut input = prompt();
        input.model = ready.judge.resource_id.to_string();
        let service = ready.fixture.service.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            service
                .execute_admitted_native(input, HeaderMap::new(), control, execution, None)
                .await
        }));
        let checked = async {
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            ensure!(judge.calls.load(Ordering::SeqCst) == 0);
            change_authority(&ready.fixture, &ready.credential, &ready.subject, change).await?;
            gate.release.cancel();
            ensure!(tokio::time::timeout(TIMEOUT, task).await??.is_err());
            drained(&ready).await?;
            ensure!(judge.calls.load(Ordering::SeqCst) == 0 && provider.calls() == 0);
            let fact = receipt(&ready, &id).await?;
            ensure!(fact["attempts"].as_array().context("attempts")?.is_empty());
            ensure!(bodies(&ready.fixture, "owned_judge_job").await?.is_empty());
            Ok(())
        }
        .await;
        gate.release.cancel();
        cleanup(&ready, &[port], judge, provider, checked).await?;
    }
    Ok(())
}
