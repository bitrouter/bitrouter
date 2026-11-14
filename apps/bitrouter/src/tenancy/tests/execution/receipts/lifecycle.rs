use super::*;
use bitrouter_ai::{
    tenancy::execution::{Outcome, ReceiptState},
    types::ApiProtocol,
};

async fn handler(
    harness: &Harness,
    deadline: tokio::time::Instant,
) -> Result<impl std::future::Future<Output = Response> + Send + 'static> {
    let service = harness.fixture.service.clone();
    let submission = service
        .submission(&harness.headers)
        .await
        .map_err(failure)?
        .narrow_deadline_for_test(deadline);
    let input =
        crate::gateway::http::parse_input(&ApiProtocol::ChatCompletions, harness.request.clone())?;
    let ingress = service
        .admission
        .ingress(serde_json::to_vec(&harness.request)?.len())
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    let headers = harness.headers.clone();
    Ok(async move {
        crate::gateway::http::handle_tenant(
            &service,
            submission,
            headers,
            ApiProtocol::ChatCompletions,
            input,
            ingress,
        )
        .await
    })
}

#[tokio::test]
async fn unpolled_sse_body_does_not_own_deadline_or_capacity_and_retains_at_most_one_chunk()
-> Result<()> {
    let harness = Harness::with_reply(
        "chat_completions",
        StatusCode::OK,
        super::wire::chat_stream(full_usage("chat_completions"), true)?,
        true,
        None,
    )
    .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let response = tokio::time::timeout(TIMEOUT, handler(&harness, deadline).await?).await?;
    ensure!(response.status() == StatusCode::OK);
    // Never poll the Body. The registry, not a consumer/provider I/O poll, owns
    // cancellation and must clear every unread authority-bearing handoff.
    tokio::time::timeout(
        Duration::from_secs(4),
        harness.fixture.service.admission.wait_drained(),
    )
    .await?;
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    let page = harness.page().await?;
    ensure!(page.execution_ids.len() == 1);
    let receipt = harness
        .receipt(&Harness::client(false)?, &page.execution_ids[0])
        .await?;
    ensure!(receipt.attempts.len() == 1);
    let technical = tokio::time::timeout(
        TIMEOUT,
        harness
            .fixture
            .service
            .technical_io
            .observed(&receipt.execution_id),
    )
    .await??;
    technical.validate_against(&receipt)?;
    ensure!(technical.operations.len() == 1 && technical.operations[0].streamed);
    let evidence_bytes = harness
        .fixture
        .service
        .read_receipt(
            harness.fixture.operator.clone(),
            harness.fixture.tenant.clone(),
            read::Request::UsageEvidence(receipt.execution_id.clone()),
        )
        .await
        .map_err(failure)?;
    let evidence: bitrouter_ai::tenancy::usage_evidence::ExecutionUsageEvidenceV1 =
        serde_json::from_slice(&evidence_bytes)?;
    bitrouter_ai::tenancy::validation::Validate::validate(&evidence)?;
    ensure!(
        evidence.execution_id == receipt.execution_id
            && evidence.installation_id == receipt.installation_id
            && evidence.tenant_id == receipt.tenant_id
            && evidence.capture == bitrouter_ai::tenancy::usage_evidence::Capture::Captured
            && evidence.attempts.len() == receipt.attempts.len()
    );
    // HTTP delivery is still cancelled/failed below. A decoder-validated provider
    // terminal is an independent immutable fact and can precede public polling.
    // Only that original retained evidence permits a non-partial source sample.
    for operation in &technical.operations {
        let closing = operation
            .attempt_ids
            .last()
            .context("actual operation closing arm")?;
        let attempt = receipt
            .attempts
            .iter()
            .find(|a| &a.attempt_id == closing)
            .context("original provider attempt")?;
        let original = evidence
            .attempts
            .iter()
            .find(|a| &a.attempt_id == closing)
            .context("original provider terminal evidence")?;
        ensure!(original.outcome == attempt.outcome);
        ensure!(
            match (&original.target, &attempt.target) {
                (Some(original), Some(attempt)) => {
                    original.provider_id == attempt.provider_id
                        && original.model_id == attempt.model_id
                        && original.account_id == attempt.account_id
                        && original.credential_id == attempt.credential_id
                        && original.credential_version == attempt.credential_version
                }
                (None, None) => true,
                _ => false,
            },
            "original provider target evidence mismatch"
        );
        if !operation.partial {
            ensure!(
                original.terminal_observed
                    && attempt.outcome == Outcome::Completed
                    && operation.upstream_duration_ms.is_some(),
                "complete technical sample has no independent successful provider terminal: terminal={}, outcome={:?}, upstream={:?}",
                original.terminal_observed,
                attempt.outcome,
                operation.upstream_duration_ms
            );
        }
    }

    ensure!(matches!(
        receipt.outcome,
        Outcome::Cancelled | Outcome::Failed
    ));
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
    ensure!(
        bytes.is_empty(),
        "unread output survived cancellation and reservation release"
    );
    tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
    Ok(())
}

#[tokio::test]
async fn aborted_http_waiter_before_provider_headers_cancels_and_retains_unknown_evidence()
-> Result<()> {
    let gate = Arc::new(Semaphore::new(0));
    let mut harness = Harness::with_reply(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
        Some(gate.clone()),
    )
    .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let caller = AbortOnDropHandle::new(tokio::spawn(handler(&harness, deadline).await?));
    tokio::time::timeout(TIMEOUT, harness.received.recv())
        .await?
        .context("provider accepted complete POST")?;
    caller.abort();
    ensure!(caller.await.is_err_and(|error| error.is_cancelled()));
    // The provider remains blocked. No client timeout or provider release can
    // explain the observed drain; the closed handoff cancels the owned request.
    tokio::time::timeout(
        Duration::from_secs(4),
        harness.fixture.service.admission.wait_drained(),
    )
    .await?;
    ensure!(gate.available_permits() == 0);
    let page = harness.page().await?;
    ensure!(page.execution_ids.len() == 1);
    let receipt = harness
        .receipt(&Harness::client(false)?, &page.execution_ids[0])
        .await?;
    ensure!(
        receipt.state == ReceiptState::Unknown
            && receipt.attempts.len() == 1
            && receipt.attempts[0].outcome == Outcome::Unknown
    );
    let technical = tokio::time::timeout(
        TIMEOUT,
        harness
            .fixture
            .service
            .technical_io
            .observed(&receipt.execution_id),
    )
    .await??;
    technical.validate_against(&receipt)?;
    ensure!(technical.operations.len() == 1 && technical.operations[0].partial);
    ensure!(
        technical.operations[0].first_semantic.is_none()
            && technical.operations[0].generation_duration_ms.is_none()
    );
    gate.add_permits(1);
    harness.fixture.service.shutdown().await?;
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    Ok(())
}
