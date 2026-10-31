//! 035 must preserve 034's independently proven output and delivery handshake.
use super::*;
use crate::tenancy::receipts::store as receipt_store;
use bitrouter_ai::tenancy::{execution::Outcome, identity::ExecutionId};

#[tokio::test]
async fn receipt_rollback_never_delivers_or_leaves_an_activated_http_continuation() -> Result<()> {
    for stream in [false, true] {
        let mut server = Server::start().await?;
        let ready = ready(&server).await?;
        ready.fixture.db.execute_unprepared("CREATE TRIGGER receipt_publication_failure BEFORE INSERT ON tenancy_execution_receipts BEGIN SELECT RAISE(ABORT, 'owned receipt fixture'); END").await?;
        let app = Router::new()
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(axum::middleware::from_fn_with_state(
                ready.fixture.service.clone(),
                crate::tenancy::http::dispatch,
            ));
        let mut request = Request::post("/v1/responses")
            .body(Body::from(serde_json::to_vec(&body(stream, None))?))?;
        *request.headers_mut() = ready.headers.clone();
        request
            .headers_mut()
            .insert("content-type", "application/json".parse()?);
        let result: Result<ExecutionId> = async {
            let response = tokio::time::timeout(WAIT, app.oneshot(request)).await??;
            let status = response.status();
            let id: ExecutionId = response
                .headers()
                .get("x-bitrouter-request-id")
                .context("admitted execution header")?
                .to_str()?
                .try_into()?;
            let bytes = tokio::time::timeout(
                WAIT,
                axum::body::to_bytes(response.into_body(), 1024 * 1024),
            )
            .await??;
            if stream {
                ensure!(status == StatusCode::OK);
                ensure!(
                    !String::from_utf8_lossy(&bytes).contains("\"type\":\"response.completed\""),
                    "success terminal escaped failed receipt commit"
                );
            } else {
                ensure!(status == StatusCode::SERVICE_UNAVAILABLE);
            }
            server.wire().await?;
            ensure!(server.state.calls.load(Ordering::SeqCst) == 1);
            ensure!(
                receipt_store::lookup(
                    &ready.fixture.db,
                    &ready.fixture.installation,
                    &ready.fixture.tenant,
                    &id
                )
                .await?
                .is_none()
            );
            ensure!(
                tokio::time::timeout(WAIT, ready.fixture.service.shutdown())
                    .await?
                    .is_err()
            );
            ensure!(
                row_count(&ready).await? == 0,
                "undelivered continuation was not compensated"
            );
            Ok(id)
        }
        .await;
        // Cleanup is attempted on both assertion outcomes; only this fixture's trigger is removed.
        let removed = ready
            .fixture
            .db
            .execute_unprepared("DROP TRIGGER receipt_publication_failure")
            .await;
        let drained = tokio::time::timeout(WAIT, ready.fixture.service.shutdown()).await;
        let closed = server.close().await;
        removed?;
        drained??;
        closed?;
        let id = result?;
        let receipt = receipt_store::lookup(
            &ready.fixture.db,
            &ready.fixture.installation,
            &ready.fixture.tenant,
            &id,
        )
        .await?
        .context("same attempted work reconciled without another inference")?;
        ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Completed);
        ensure!(receipt.usage.input_tokens == Some(4) && receipt.usage.output_tokens == Some(2));
        ensure!(row_count(&ready).await? == 0);
    }
    Ok(())
}

struct ReplaceText;
#[async_trait::async_trait]
impl crate::gateway::pipeline::hooks::StreamHook for ReplaceText {
    fn interest(&self) -> crate::gateway::pipeline::stream::StreamInterest {
        crate::gateway::pipeline::stream::StreamInterest::none().with_text_delta()
    }
    async fn on_part(
        &self,
        _ctx: &mut crate::gateway::pipeline::context::StreamContext,
        mut part: bitrouter_ai::types::StreamPart,
    ) -> crate::gateway::error::Result<crate::gateway::pipeline::stream::StreamAction> {
        if let bitrouter_ai::types::StreamPart::TextDelta { text, .. } = &mut part {
            *text = "locally rewritten answer".into();
        }
        Ok(crate::gateway::pipeline::stream::StreamAction::Replace(
            vec![part],
        ))
    }
    async fn on_stream_end(
        &self,
        _ctx: &mut crate::gateway::pipeline::context::StreamContext,
        _outcome: &crate::gateway::pipeline::stream::StreamOutcome,
    ) -> crate::gateway::error::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn rootless_post_hook_rewrite_cannot_mint_proof_and_does_not_erase_paid_usage() -> Result<()>
{
    use crate::gateway::pipeline::{builder::PipelineBuilder, types::PipelineRequest};
    use bitrouter_ai::types::ApiProtocol;
    use futures::StreamExt;
    for rewrite in [false, true] {
        let mut server = Server::start().await?;
        let ready = ready(&server).await?;
        let result: Result<()> = async {
            let submission = ready.fixture.service.submission(&ready.headers).map_err(failure)?;
            let execution = ready.fixture.service.admit_execution(submission, MODEL, &ApiProtocol::Responses).await.map_err(failure)?;
            let id = execution.id().clone();
            // This test-only profile adds one real output hook to the retained route,
            // real executor and unchanged 034 runtime. It supplies no attempt proof.
            let runtime = crate::continuation::ContinuationRuntime::for_tenant(ready.fixture.service.http_registry.clone(), execution.clone())?;
            let mut builder = PipelineBuilder::new();
            builder.owned_drain().routing_table(execution.route().clone()).executor(ready.fixture.service.executor.clone())
                .pre_resolution_hook(runtime.clone()).route_hook(runtime.clone()).required_finalizer(runtime);
            crate::tenancy::routing::install(&mut builder, &execution, ready.fixture.service.routing.clone());
            if rewrite { builder.stream_hook(ReplaceText); }
            let pipeline = Arc::new(builder.build()?);
            let mut input = body(true, None);
            input["input"] = json!([{"role":"user","content":[{"type":"input_text","text":"describe this"},{"type":"input_image","image_url":"data:image/png;base64,aW1hZ2U="}]}]);
            let input = crate::gateway::http::parse_input(&ApiProtocol::Responses, input)?;
            let prompt = input.generation_prompt().context("generation input")?.clone();
            let mut request = PipelineRequest::new(MODEL, execution.caller(), prompt);
            request.request_id = id.to_string();
            request.headers = ready.headers.clone();
            request.inbound_protocol = Some(ApiProtocol::Responses);
            execution.attach(pipeline.clone(), &mut request)?;
            let mut parts = pipeline.execute_stream(request).await?;
            let mut rejected = false;
            let mut rewritten = false;
            while let Some(part) = tokio::time::timeout(WAIT, parts.next()).await? {
                match part {
                    Ok(bitrouter_ai::types::StreamPart::TextDelta { text, .. }) => rewritten |= text == "locally rewritten answer",
                    Err(_) => rejected = true,
                    _ => {},
                }
            }
            drop(parts);
            let wire = server.wire().await?;
            ensure!(wire.to_string().contains("input_image"));
            ensure!(rewritten == rewrite);
            ensure!(rejected == rewrite, "post-hook proof comparison was bypassed");
            // Required-finalizer rejection may retain failed settlement status;
            // receipt persistence is still an independent mandatory obligation.
            let receipt = tokio::time::timeout(WAIT, async {
                loop {
                    if let Some(receipt) = receipt_store::lookup(&ready.fixture.db, &ready.fixture.installation, &ready.fixture.tenant, &id).await? {
                        break Ok::<_, anyhow::Error>(receipt);
                    }
                    tokio::task::yield_now().await;
                }
            }).await??;
            ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Completed);
            ensure!(receipt.usage.input_tokens == Some(4) && receipt.usage.output_tokens == Some(2));
            ensure!(row_count(&ready).await? == if rewrite { 0 } else { 1 });
            ensure!(server.state.calls.load(Ordering::SeqCst) == 1);
            drop(execution);
            Ok(())
        }.await;
        let cleanup = tokio::time::timeout(WAIT, ready.fixture.service.shutdown()).await;
        let closed = server.close().await;
        // A genuine finalizer error may deliberately retain a failed owner. It
        // must still be bounded and may not fabricate clean cleanup.
        if rewrite {
            let _ = cleanup?;
        } else {
            cleanup??;
        }
        closed?;
        result?;
    }
    Ok(())
}
