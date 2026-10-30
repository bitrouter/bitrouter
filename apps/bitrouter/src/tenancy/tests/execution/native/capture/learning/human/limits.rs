use super::*;

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn held_paid_terminal_keeps_the_last_history_row_and_review_remains_available() -> Result<()>
{
    let mut host = Host::new(true, 1).await?;
    let result = async {
        let native = host.open().await?;
        let current = host.current()?;
        current.prompt(&native, "reserved history row").await?;
        drained(&host.ready).await?;
        let checkpoint = current.checkpoint(&native);
        tokio::pin!(checkpoint);
        tokio::select! {
            reply = &mut checkpoint => anyhow::bail!("judge did not stay held: {reply:?}"),
            result = tokio::time::timeout(TIMEOUT, async {
                while host.judge.calls.load(Ordering::SeqCst) == 0 { tokio::task::yield_now().await; }
            }) => { result?; }
        }
        let mut view = current.review_checkpoint(&native, None).await?;
        let mut last = None;
        for index in 0..127 {
            let input = submission(&view, &format!("human-capacity-{index:03}"));
            let reply = current.submit_assessment(&native, input.clone()).await?;
            last = Some((input, reply));
            view = current.review_checkpoint(&native, None).await?;
            ensure!(view.history.len() == index + 1);
        }
        let next = current.submit_assessment(&native, submission(&view, "cannot-consume-paid-row")).await
            .err().context("manual consumed paid terminal row")?;
        rejected(&next, ErrorCode::CapacityExceeded)?;
        let (input, original) = last.context("no actual manual rows")?;
        let replay = current.submit_assessment(&native, input).await?;
        ensure!(serde_json::to_value(replay)? == serde_json::to_value(&original)?);
        ensure!(current.review_checkpoint(&native, None).await?.history.len() == 127
            && host.ready.fixture.service.current().is_ok());
        host.judge.release.cancel();
        tokio::time::timeout(TIMEOUT, &mut checkpoint).await??;
        drained(&host.ready).await?;
        let final_view = current.review_checkpoint(&native, None).await?;
        ensure!(final_view.history.len() == 128
            && final_view.scoring.expected_revision.as_deref() == Some(original.revision.revision_id.as_str())
            && final_view.history.iter().filter(|value| value.input.source == AssessmentSource::Agentic).count() == 1);
        ensure!(bodies(&host.ready.fixture, "owned_judge_pending").await?.is_empty()
            && bodies(&host.ready.fixture, "owned_judge_budget").await?[0]["accepted"] == 1);
        let job = bodies(&host.ready.fixture, "owned_judge_job").await?;
        let facts = receipt(&host.ready, job[0]["execution"].as_str().context("execution")?).await?;
        ensure!(facts["usage"]["input_tokens"] == 4 && facts["usage"]["output_tokens"] == 2);
        Ok(())
    }.await;
    host.finish(result).await
}

fn substantial(view: &CheckpointReview, index: usize) -> RubricSubmission {
    let mut input = submission(view, &format!("bounded-large-{index:03}"));
    // Fixed-size valid free text, well below the unchanged512KiB input and
    //1MiB revision bounds. Every retained row is produced by the public actor.
    for item in &mut input.evaluation.items {
        item.selection_reason = "s".repeat(30 * 1024);
        item.explanation = "e".repeat(30 * 1024);
    }
    input
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn held_paid_byte_reservation_refuses_manual_growth_before_the_terminal() -> Result<()> {
    let mut host = Host::new(true, 1).await?;
    let result = async {
        let native = host.open().await?;
        let current = host.current()?;
        current.prompt(&native, "reserved byte capacity").await?;
        drained(&host.ready).await?;
        let checkpoint = current.checkpoint(&native);
        tokio::pin!(checkpoint);
        tokio::select! {
            reply = &mut checkpoint => anyhow::bail!("judge was not held: {reply:?}"),
            result = tokio::time::timeout(TIMEOUT, async {
                while host.judge.calls.load(Ordering::SeqCst) == 0 { tokio::task::yield_now().await; }
            }) => { result?; }
        }
        let mut view = current.review_checkpoint(&native, None).await?;
        let mut rejected_at = None;
        let mut last_size = 0usize;
        for index in 0..128 {
            let input = substantial(&view, index);
            ensure!(serde_json::to_vec(&input)?.len() < 512 * 1024);
            match current.submit_assessment(&native, input).await {
                Ok(receipt) => {
                    last_size = serde_json::to_vec(&receipt.revision)?.len();
                    ensure!(last_size < 1024 * 1024);
                    view = current.review_checkpoint(&native, None).await?;
                }
                Err(error) => { rejected(&error, ErrorCode::CapacityExceeded)?; rejected_at = Some(index); break; }
            }
        }
        let index = rejected_at.context("history did not reach its byte bound")?;
        let bytes = view.history.iter().try_fold(0usize, |total, value| -> Result<usize> {
            total.checked_add(serde_json::to_vec(value)?.len()).context("fixture byte overflow")
        })?;
        ensure!(index > 1 && index < 127 && bytes + last_size + 1024 * 1024 > 8 * 1024 * 1024
            && bytes + 1024 * 1024 <= 8 * 1024 * 1024,
            "the actual cumulative paid reservation was not the limiting boundary");
        host.judge.release.cancel();
        tokio::time::timeout(TIMEOUT, &mut checkpoint).await??;
        drained(&host.ready).await?;
        let final_view = current.review_checkpoint(&native, None).await?;
        ensure!(final_view.history.len() == view.history.len() + 1
            && final_view.scoring.expected_revision == view.scoring.expected_revision);
        ensure!(bodies(&host.ready.fixture, "owned_judge_pending").await?.is_empty());
        Ok(())
    }.await;
    host.finish(result).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn full_manual_history_has_no_hidden_paid_reservation_and_replay_does_not_grow_it()
-> Result<()> {
    let mut host = Host::new(false, usize::MAX).await?;
    let result = async {
        let native = host.open().await?;
        let current = host.current()?;
        current.prompt(&native, "no-paid control").await?;
        current.checkpoint(&native).await?;
        let mut view = current.review_checkpoint(&native, None).await?;
        let mut last = None;
        for index in 0..128 {
            let input = submission(&view, &format!("no-paid-{index:03}"));
            let revision = current.submit_assessment(&native, input.clone()).await?;
            last = Some((input, revision));
            view = current.review_checkpoint(&native, None).await?;
        }
        ensure!(view.history.len() == 128);
        rejected(
            &current
                .submit_assessment(&native, submission(&view, "row129"))
                .await
                .err()
                .context("row129 was accepted")?,
            ErrorCode::CapacityExceeded,
        )?;
        let (input, original) = last.context("no last revision")?;
        ensure!(
            serde_json::to_value(current.submit_assessment(&native, input).await?)?
                == serde_json::to_value(original)?
        );
        ensure!(
            current
                .review_checkpoint(&native, None)
                .await?
                .history
                .len()
                == 128
                && host.judge.calls.load(Ordering::SeqCst) == 0
                && host.ready.fixture.service.current().is_ok()
        );
        Ok(())
    }
    .await;
    host.finish(result).await
}
