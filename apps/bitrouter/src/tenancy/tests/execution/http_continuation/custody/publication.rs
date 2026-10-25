//! Actual provider completion is distinct from permission to start another send.
use super::*;
use crate::{
    gateway::{
        error::Result as GatewayResult,
        model::protocol::{InboundAdapter, responses::ResponsesAdapter},
        pipeline::{
            execution::{DeliveryPermit, Pipeline},
            operations::HookRegistration,
            settlement::{SettlementContext, SettlementRecorder},
            types::PipelineRequest,
        },
    },
    tenancy::{
        command::Command,
        tests::issued,
        transaction::{Disposition, Phase},
    },
};
use async_trait::async_trait;
use bitrouter_ai::{
    tenancy::{
        authority::{CredentialIssued, TenantPut, TenantState},
        operation::Mutation,
    },
    types::{ApiProtocol, OperationScope},
};
use futures::StreamExt;

fn revoke(ready: &Ready, credential: &CredentialIssued) -> Result<Command> {
    Ok(Command::Revoke(
        ready.fixture.tenant.clone(),
        credential.credential_id.clone(),
        Mutation {
            operation_id: "publication-revoke".try_into()?,
            expected_revision: credential.credential_revision.clone(),
            installation_id: ready.fixture.installation.clone(),
        },
    ))
}

fn lifecycle(ready: &Ready, state: TenantState) -> Result<Command> {
    Ok(Command::Tenant(
        ready.fixture.tenant.clone(),
        TenantPut {
            operation_id: "publication-lifecycle".try_into()?,
            expected_revision: Some(
                ready
                    .fixture
                    .service
                    .current()
                    .map_err(failure)?
                    .tenants
                    .get(&ready.fixture.tenant)
                    .context("tenant")?
                    .revision
                    .clone(),
            ),
            installation_id: ready.fixture.installation.clone(),
            state,
        },
    ))
}

fn unpause(server: &Server, pause: &tokio::sync::Notify) -> Result<()> {
    *server
        .state
        .pause
        .lock()
        .map_err(|_| anyhow::anyhow!("pause lock"))? = None;
    pause.notify_one();
    Ok(())
}

pub(super) async fn held_caller_change(rotate: bool) -> Result<()> {
    for stream in [false, true] {
        let mut server = Server::start().await?;
        let mut ready = ready(&server).await?;
        let credential = ready.fixture.issue("completion-caller").await?;
        ready.headers = headers(
            &ready.fixture,
            credential.secret.as_deref().context("secret")?,
            &ready.revision.revision_id,
        )?;
        let pause = Arc::new(tokio::sync::Notify::new());
        *server
            .state
            .pause
            .lock()
            .map_err(|_| anyhow::anyhow!("pause lock"))? = Some(pause.clone());
        let mut request = Box::pin(send(&ready, ready.headers.clone(), body(stream, None)));
        let reached = tokio::time::timeout(WAIT, async {
            tokio::select! {
                result = &mut request => { let _ = result?; anyhow::bail!("provider was not held"); }
                result = server.wire() => result,
            }
        }).await;
        let changed: Result<CredentialIssued> = async {
            if rotate {
                let mut input = ready.fixture.issuance("completion-rotation")?;
                input.rotate_credential_id = Some(credential.credential_id.clone());
                issued(
                    ready
                        .fixture
                        .service
                        .execute(
                            ready.fixture.operator.clone(),
                            Command::Issue(ready.fixture.tenant.clone(), input),
                        )
                        .await
                        .map_err(failure)?,
                )
            } else {
                ready
                    .fixture
                    .service
                    .execute(ready.fixture.operator.clone(), revoke(&ready, &credential)?)
                    .await
                    .map_err(failure)?;
                ready.fixture.issue("completion-replacement").await
            }
        }
        .await;
        unpause(&server, &pause)?;
        let outcome: Result<()> = async {
            ensure!(reached??.get("previous_response_id").is_none());
            let replacement = changed?;
            ensure!(
                ready
                    .fixture
                    .service
                    .authenticate(credential.secret.as_deref().context("old key")?)
                    .is_err()
            );
            let (status, bytes) = tokio::time::timeout(WAIT, request).await??;
            ensure!(status == StatusCode::OK);
            let public = public_id(&bytes, stream)?; // Includes exact observed4/2 usage.
            ensure!(String::from_utf8_lossy(&bytes).contains("owned reply"));
            let (crypto, row) = super::super::store::retained(&ready, &public).await?;
            let payload = row.open(&crypto)?;
            ensure!(payload.owner == super::super::store::owner(&ready));
            ensure!(row.metadata.publication_state == "active");
            let live = payload.live.context("actual terminal binding")?;
            ensure!(live.revision == ready.revision.revision_id);
            ensure!(live.provider_response_id == "resp-http-private-1");
            ensure!(row_count(&ready).await? == 1);
            for previous in [None, Some(public.as_str())] {
                let (status, _) =
                    send(&ready, ready.headers.clone(), body(false, previous)).await?;
                ensure!(!status.is_success());
            }
            ensure!(server.state.calls.load(Ordering::SeqCst) == 1);
            let replacement_headers = headers(
                &ready.fixture,
                replacement.secret.as_deref().context("replacement")?,
                &ready.revision.revision_id,
            )?;
            let (status, bytes) =
                send(&ready, replacement_headers, body(!stream, Some(&public))).await?;
            ensure!(status == StatusCode::OK);
            public_id(&bytes, !stream)?;
            ensure!(server.wire().await?["previous_response_id"] == "resp-http-private-1");
            ensure!(row_count(&ready).await? == 2);
            ready.fixture.service.cleanup.ensure_clean()?;
            Ok(())
        }
        .await;
        ready.fixture.service.shutdown().await?;
        server.close().await?;
        outcome?;
    }
    Ok(())
}

#[tokio::test]
async fn rotation_during_provider_wait_keeps_owned_completion_and_requires_new_key() -> Result<()> {
    held_caller_change(true).await
}

#[tokio::test]
async fn queued_publication_separates_caller_revoke_from_tenant_inactivity() -> Result<()> {
    for inactive in [
        None,
        Some(TenantState::Suspended),
        Some(TenantState::Deleting),
    ] {
        let mut server = Server::start().await?;
        let mut ready = ready(&server).await?;
        let credential = ready.fixture.issue("queued-publication-caller").await?;
        ready.headers = headers(
            &ready.fixture,
            credential.secret.as_deref().context("secret")?,
            &ready.revision.revision_id,
        )?;
        let pause = Arc::new(tokio::sync::Notify::new());
        *server
            .state
            .pause
            .lock()
            .map_err(|_| anyhow::anyhow!("pause lock"))? = Some(pause.clone());
        let mut request = Box::pin(send(&ready, ready.headers.clone(), body(false, None)));
        let reached = tokio::time::timeout(WAIT, async {
            tokio::select! {
                result = &mut request => { let _ = result?; anyhow::bail!("provider was not held"); }
                result = server.wire() => result,
            }
        }).await;
        let command = match inactive {
            Some(state) => lifecycle(&ready, state)?,
            None => revoke(&ready, &credential)?,
        };
        // Hold the real safety command before its commit. The provider terminal
        // then queues Prepare on the ordinary lane behind that acknowledgement.
        let gate = ready.fixture.service.cleanup.hold(Phase::Committing);
        let mut mutation = Box::pin(
            ready
                .fixture
                .service
                .execute(ready.fixture.operator.clone(), command),
        );
        let held = tokio::time::timeout(WAIT, async {
            tokio::select! {
                result = &mut mutation => { let _ = result.map_err(failure)?; anyhow::bail!("mutation bypassed gate"); }
                () = gate.entered.notified() => Ok::<_, anyhow::Error>(()),
            }
        }).await;
        let capacity = ready.fixture.service.commands.capacity();
        unpause(&server, &pause)?;
        let queued = tokio::time::timeout(WAIT, async {
            loop {
                tokio::select! {
                    result = &mut request => { let _ = result?; anyhow::bail!("publication did not queue behind mutation"); }
                    () = tokio::task::yield_now() => {}
                }
                if ready.fixture.service.commands.capacity() < capacity { return Ok::<_, anyhow::Error>(()); }
            }
        }).await;
        ready.fixture.service.cleanup.release_gate();
        let outcome: Result<()> = async {
            reached??;
            held??;
            queued??;
            tokio::time::timeout(WAIT, mutation)
                .await?
                .map_err(failure)?;
            let (status, bytes) = tokio::time::timeout(WAIT, request).await??;
            if inactive.is_some() {
                ensure!(!status.is_success());
                ensure!(row_count(&ready).await? == 0);
            } else {
                ensure!(status == StatusCode::OK);
                let public = public_id(&bytes, false)?;
                ensure!(row_count(&ready).await? == 1);
                let (status, _) =
                    send(&ready, ready.headers.clone(), body(false, Some(&public))).await?;
                ensure!(!status.is_success());
            }
            ensure!(server.state.calls.load(Ordering::SeqCst) == 1);
            ensure!(
                ready
                    .fixture
                    .service
                    .http_continuation
                    .state
                    .pending_tickets()
                    .map_err(|_| anyhow::anyhow!("pending"))?
                    .is_empty()
            );
            ensure!(ready.fixture.service.current().is_ok());
            ready.fixture.service.cleanup.ensure_clean()?;
            Ok(())
        }
        .await;
        ready.fixture.service.shutdown().await?;
        server.close().await?;
        outcome?;
    }
    Ok(())
}

#[derive(Clone, Default)]
struct Meter(Arc<tokio::sync::Mutex<Vec<(u64, u64)>>>);
#[async_trait]
impl SettlementRecorder for Meter {
    async fn record(&self, ctx: &mut SettlementContext) -> GatewayResult<()> {
        self.0
            .lock()
            .await
            .push((ctx.prompt_tokens, ctx.completion_tokens));
        Ok(())
    }
}
struct Call {
    pipeline: Arc<Pipeline>,
    request: PipelineRequest,
    meter: Meter,
    public: String,
}
async fn call(ready: &Ready, stream: bool) -> Result<Call> {
    let execution = super::admitted(ready).await?;
    let mut pipeline = execution.pipeline()?;
    let meter = Meter::default();
    Arc::get_mut(&mut pipeline)
        .context("unregistered isolated pipeline")?
        .settlement_recorders
        .push(HookRegistration::new(
            Arc::new(meter.clone()),
            OperationScope::Generation,
            std::any::TypeId::of::<Meter>(),
        ));
    let prompt = ResponsesAdapter.parse_request(body(stream, None))?;
    let mut request = PipelineRequest::new(MODEL, execution.caller(), prompt);
    request.request_id = execution.id().to_string();
    request.original_model = MODEL.into();
    request.headers = ready.headers.clone();
    request.inbound_protocol = Some(ApiProtocol::Responses);
    let public = encode_gateway_continuation_id(&request.request_id)?;
    execution.attach(pipeline.clone(), &mut request)?;
    Ok(Call {
        pipeline,
        request,
        meter,
        public,
    })
}

#[tokio::test]
async fn inactive_tenant_cannot_activate_delivering_mapping_and_usage_survives() -> Result<()> {
    for stream in [false, true] {
        let mut server = Server::start().await?;
        let ready = ready(&server).await?;
        let outcome: Result<()> = async {
            let Call {
                pipeline,
                request,
                meter,
                public,
            } = call(&ready, stream).await?;
            let delivery: DeliveryPermit = if stream {
                let mut prepared = pipeline.clone().execute_stream_prepared(request).await?;
                tokio::time::timeout(WAIT, async {
                    while let Some(part) = prepared.parts.next().await {
                        if let Some(delivery) = part?.delivery {
                            return Ok::<_, anyhow::Error>(delivery);
                        }
                    }
                    anyhow::bail!("no held successful terminal")
                })
                .await??
            } else {
                pipeline
                    .clone()
                    .execute_detached_prepared(request)
                    .await?
                    .delivery
            };
            server.wire().await?;
            // Actual Prepare and Delivering have committed. The real delivery
            // permit is deliberately unpolled, so Active cannot be queued yet.
            tokio::time::timeout(WAIT, async {
                loop {
                    let (_, row) = super::super::store::retained(&ready, &public).await?;
                    if row.metadata.publication_state == "delivering" {
                        return Ok::<_, anyhow::Error>(());
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await??;
            ready
                .fixture
                .service
                .execute(
                    ready.fixture.operator.clone(),
                    lifecycle(&ready, TenantState::Suspended)?,
                )
                .await
                .map_err(failure)?;
            ensure!(
                tokio::time::timeout(WAIT, delivery.deliver())
                    .await?
                    .is_err()
            );
            drop(pipeline);
            tokio::time::timeout(WAIT, ready.fixture.service.admission.wait_drained()).await?;
            ensure!(meter.0.lock().await.as_slice() == [(4, 2)]);
            ensure!(row_count(&ready).await? == 0);
            ensure!(server.state.calls.load(Ordering::SeqCst) == 1);
            ensure!(ready.fixture.service.current().is_ok());
            ready.fixture.service.cleanup.ensure_clean()?;
            Ok(())
        }
        .await;
        ready.fixture.service.shutdown().await?;
        server.close().await?;
        outcome?;
    }
    Ok(())
}

#[tokio::test]
async fn lost_prepare_reply_keeps_request_failure_and_provider_usage_after_confirmed_cleanup()
-> Result<()> {
    for stream in [false, true] {
        let mut server = Server::start().await?;
        let ready = ready(&server).await?;
        let outcome: Result<()> = async {
            let Call {
                pipeline,
                request,
                meter,
                public: _,
            } = call(&ready, stream).await?;
            ready
                .fixture
                .service
                .http_continuation
                .state
                .lose_next_prepare_reply();
            if stream {
                let mut output = pipeline.clone().execute_stream(request).await?;
                let failed = tokio::time::timeout(WAIT, async {
                    let mut failed = false;
                    while let Some(part) = output.next().await {
                        failed |= part.is_err();
                    }
                    failed
                })
                .await?;
                ensure!(failed);
            } else {
                ensure!(
                    pipeline
                        .clone()
                        .execute_detached_prepared(request)
                        .await
                        .is_err()
                );
            }
            server.wire().await?;
            drop(pipeline);
            tokio::time::timeout(WAIT, ready.fixture.service.admission.wait_drained()).await?;
            ensure!(meter.0.lock().await.as_slice() == [(4, 2)]);
            ensure!(row_count(&ready).await? == 0);
            ensure!(
                ready
                    .fixture
                    .service
                    .http_continuation
                    .state
                    .pending_tickets()
                    .map_err(|_| anyhow::anyhow!("pending"))?
                    .is_empty()
            );
            ready.fixture.service.cleanup.ensure_clean()?;
            Ok(())
        }
        .await;
        ready.fixture.service.shutdown().await?;
        server.close().await?;
        outcome?;
    }
    Ok(())
}

#[tokio::test]
async fn compensation_ack_is_exact_and_cancelled_waiter_cannot_release_pending_custody()
-> Result<()> {
    let server = Server::start().await?;
    let ready = ready(&server).await?;
    let port = ready.fixture.service.http_continuation.clone();
    let ticket = port
        .state
        .reserve(
            super::super::store::owner(&ready),
            "ack-execution".try_into()?,
            encode_gateway_continuation_id("ack-request")?,
            9001,
        )
        .map_err(|code| anyhow::anyhow!("reservation {code:?}"))?;
    let gate = ready.fixture.service.cleanup.hold(Phase::RollingBack);
    let mut cleanup =
        Box::pin(port.compensate_confirmed(ticket.clone(), tokio::time::Instant::now() + WAIT));
    let entered = tokio::time::timeout(WAIT, async {
        tokio::select! {
            result = &mut cleanup => { let _ = result; anyhow::bail!("cleanup bypassed H gate"); }
            () = gate.entered.notified() => Ok::<_, anyhow::Error>(()),
        }
    })
    .await;
    drop(cleanup);
    let held = port.state.matches(&ticket)
        && ready.fixture.service.cleanup.disposition() == Disposition::Pending;
    ready.fixture.service.cleanup.release_gate();
    let outcome: Result<()> = async {
        entered??;
        ensure!(held, "lost waiter forged acknowledged release");
        let ack = port
            .compensate_confirmed(ticket.clone(), tokio::time::Instant::now() + WAIT)
            .await
            .map_err(|code| anyhow::anyhow!("confirmed cleanup {code:?}"))?;
        ensure!(ack.matches(&port, &ticket));
        for change in ["generation", "execution", "delivery", "owner"] {
            let mut other = ticket.clone();
            match change {
                "generation" => other.generation = uuid::Uuid::new_v4().to_string(),
                "execution" => other.execution = "different-execution".try_into()?,
                "delivery" => other.delivery += 1,
                _ => other.owner.principal = "different-principal".try_into()?,
            }
            ensure!(!ack.matches(&port, &other));
        }
        ensure!(!port.state.matches(&ticket));
        ready.fixture.service.cleanup.ensure_clean()?;
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    server.close().await?;
    outcome
}

#[tokio::test]
async fn different_receipt_cannot_claim_the_actual_actor_cleanup() -> Result<()> {
    use crate::{
        continuation::{ContinuationRuntime, tenant::runtime::Runtime},
        gateway::pipeline::{
            context::PipelineContext, hooks::PreRequestHook, settlement::RequiredFinalizer,
        },
    };
    let server = Server::start().await?;
    let ready = ready(&server).await?;
    let outcome: Result<()> = async {
        let execution = super::admitted(&ready).await?;
        let runtime = ContinuationRuntime::for_tenant(
            ready.fixture.service.http_registry.clone(),
            execution.clone(),
        )?;
        let mut request = PipelineRequest::new(
            MODEL,
            execution.caller(),
            ResponsesAdapter.parse_request(body(false, None))?,
        );
        request.request_id = execution.id().to_string();
        request.original_model = MODEL.into();
        request.headers = ready.headers.clone();
        request.inbound_protocol = Some(ApiProtocol::Responses);
        let mut ctx = PipelineContext::new(request);
        ctx.insert_extension(execution.clone());
        runtime.check(&mut ctx).await?;
        let owned = ctx
            .extension::<Runtime>()
            .context("actual private runtime")?;
        let receipt = RequiredFinalizationReceipt::new();
        let different = RequiredFinalizationReceipt::new();
        let finalization = ctx.required_finalization_context(false);
        // This receipt-only negative test has no executor terminal proof and
        // therefore cannot mint a mapping. Both rollback calls use the real H
        // actor; a generic successful return or a foreign receipt is insufficient.
        runtime
            .finalize_with_receipt(&finalization, &receipt)
            .await?;
        runtime
            .rollback_with_receipt(&finalization, &different)
            .await?;
        ensure!(!owned.confirmed_compensation(&receipt));
        ensure!(!owned.confirmed_compensation(&different));
        runtime
            .rollback_with_receipt(&finalization, &receipt)
            .await?;
        ensure!(owned.confirmed_compensation(&receipt));
        ensure!(!owned.confirmed_compensation(&different));
        ensure!(row_count(&ready).await? == 0);
        ensure!(server.state.calls.load(Ordering::SeqCst) == 0);
        drop((owned, ctx, runtime, execution));
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    server.close().await?;
    outcome
}
