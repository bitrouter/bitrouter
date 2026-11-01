use super::*;

mod mailbox;
mod publication;
use crate::{
    continuation::tenant::state::{Reservation, State as PublicationState},
    gateway::{
        model::protocol::responses::encode_gateway_continuation_id,
        pipeline::settlement::RequiredFinalizationReceipt,
    },
    tenancy::continuation::http::codec::Owner,
};
use bitrouter_ai::tenancy::operation::ErrorCode;

async fn fixture() -> Result<(Ready, Server, Arc<PublicationState>, Owner)> {
    let server = Server::start().await?;
    let ready = ready(&server).await?;
    let state = PublicationState::new(&Default::default(), uuid::Uuid::new_v4().to_string())?;
    state
        .initialize(Some(
            ready
                .fixture
                .service
                .http_continuation
                .state
                .crypto()
                .map_err(|_| anyhow::anyhow!("pinned crypto"))?
                .clone(),
        ))
        .map_err(|_| anyhow::anyhow!("initialize marker fixture"))?;
    let owner = Owner {
        installation: ready.fixture.installation.clone(),
        tenant: ready.fixture.tenant.clone(),
        principal: ready.fixture.principal.clone(),
        route: ready.route.clone(),
    };
    Ok((ready, server, state, owner))
}

#[tokio::test]
async fn completed_http_shutdown_is_repeatable_without_a_live_actor() -> Result<()> {
    let mut provider = Server::start().await?;
    let ready = ready(&provider).await?;
    let outcome: Result<()> = async {
        let (status, bytes) = send(&ready, ready.headers.clone(), body(false, None)).await?;
        ensure!(status == StatusCode::OK);
        public_id(&bytes, false)?;
        provider.wire().await?;
        ready.fixture.service.shutdown().await?;
        ensure!(ready.fixture.service.commands.is_closed());
        ensure!(ready.fixture.service.http_continuation.state.barrier());
        // Teardown owners retain clones. Reusing a known barrier is safe only
        // because the producer gate is permanently shut and H still checks clean.
        ready.fixture.service.clone().shutdown().await?;
        ready.fixture.service.cleanup.ensure_clean()?;
        ensure!(row_count(&ready).await? == 1);
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    provider.close().await?;
    outcome
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_shutdown_after_actor_stop_reuses_acknowledged_barrier() -> Result<()> {
    let provider = Server::start().await?;
    let ready = ready(&provider).await?;
    let mut shutdown = Box::pin(ready.fixture.service.shutdown());
    let reached = tokio::time::timeout(
        WAIT,
        std::future::poll_fn(
            |cx| match std::future::Future::poll(shutdown.as_mut(), cx) {
                std::task::Poll::Ready(_) => std::task::Poll::Ready(Err(anyhow::anyhow!(
                    "shutdown did not suspend at authority join"
                ))),
                std::task::Poll::Pending if ready.fixture.service.stop.is_cancelled() => {
                    std::task::Poll::Ready(Ok(()))
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            },
        ),
    )
    .await;
    drop(shutdown);
    let outcome: Result<()> = async {
        reached??;
        ensure!(ready.fixture.service.http_continuation.state.barrier());
        tokio::time::timeout(WAIT, ready.fixture.service.completion.clone())
            .await?
            .map_err(|_| anyhow::anyhow!("authority join"))?;
        ready.fixture.service.cleanup.ensure_clean()?;
        ready.fixture.service.shutdown().await?;
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    provider.close().await?;
    outcome
}

#[tokio::test]
async fn losing_queued_waiter_retains_marker_until_acknowledged_release() -> Result<()> {
    let (ready, server, state, owner) = fixture().await?;
    // These are marker state-machine tests, not substitutes for H fault gates.
    // Crypto still comes from the actual acknowledged bootstrap, not a setter.
    let result: Result<()> = async {
        let execution = "marker-execution".try_into()?;
        let ticket = state
            .reserve(
                owner.clone(),
                execution,
                encode_gateway_continuation_id("marker-request")?,
                101,
            )
            .map_err(|_| anyhow::anyhow!("reserve"))?;
        let reservation = Reservation {
            state: state.clone(),
            ticket: ticket.clone(),
        };
        state
            .queue(&ticket, &RequiredFinalizationReceipt::new())
            .map_err(|_| anyhow::anyhow!("queue"))?;
        drop(reservation);
        ensure!(state.matches(&ticket));
        ensure!(
            state
                .hides(&ticket.physical)
                .map_err(|_| anyhow::anyhow!("hidden"))?
        );
        // A later acknowledged commit cannot erase the earlier abandonment.
        state.committed(&ticket, "2100-01-01T00:00:00.000000Z");
        ensure!(state.matches(&ticket));
        let mut wrong = ticket.clone();
        wrong.generation = uuid::Uuid::new_v4().to_string();
        state.release(&wrong);
        ensure!(state.matches(&ticket));
        state.release(&ticket);
        ensure!(
            !state
                .pending_for(&ticket.execution)
                .map_err(|_| anyhow::anyhow!("drained"))?
        );
        let unqueued = state
            .reserve(
                owner,
                "unqueued-execution".try_into()?,
                encode_gateway_continuation_id("unqueued-request")?,
                102,
            )
            .map_err(|_| anyhow::anyhow!("reserve unqueued"))?;
        drop(Reservation {
            state: state.clone(),
            ticket: unqueued.clone(),
        });
        ensure!(!state.matches(&unqueued));
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    server.close().await?;
    result
}

#[tokio::test]
async fn pending_reservations_are_bounded_and_close_does_not_forge_cleanliness() -> Result<()> {
    let (ready, server, state, owner) = fixture().await?;
    let result: Result<()> = async {
        let mut tickets = Vec::new();
        for id in 0..256_u64 {
            let ticket = state
                .reserve(
                    owner.clone(),
                    format!("execution-{id}").try_into()?,
                    encode_gateway_continuation_id(&format!("request-{id}"))?,
                    id,
                )
                .map_err(|_| anyhow::anyhow!("bounded reservation"))?;
            tickets.push(ticket);
        }
        ensure!(matches!(
            state.reserve(
                owner.clone(),
                "overflow".try_into()?,
                encode_gateway_continuation_id("overflow")?,
                256
            ),
            Err(ErrorCode::CapacityExceeded)
        ));
        let first = tickets.first().context("first reservation")?;
        state
            .queue(first, &RequiredFinalizationReceipt::new())
            .map_err(|_| anyhow::anyhow!("queue"))?;
        state.abandon(first);
        state.close();
        state.acknowledge_barrier();
        ensure!(
            state.matches(first),
            "producer close cannot settle queued SQL"
        );
        ensure!(
            state
                .queue(
                    tickets.get(1).context("second reservation")?,
                    &RequiredFinalizationReceipt::new()
                )
                .is_err()
        );
        for ticket in &tickets {
            state.release(ticket);
        }
        ensure!(
            !state
                .pending_for(&first.execution)
                .map_err(|_| anyhow::anyhow!("drained"))?
        );
        ensure!(
            state
                .reserve(
                    owner,
                    "after-close".try_into()?,
                    encode_gateway_continuation_id("after-close")?,
                    300
                )
                .is_err()
        );
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    server.close().await?;
    result
}

#[tokio::test]
async fn actual_http_disconnect_during_commit_retains_then_compensates_generation() -> Result<()> {
    use crate::tenancy::transaction::{Disposition, Phase};
    let mut server = Server::start().await?;
    let ready = ready(&server).await?;
    // Empty maintenance transactions only roll back. The first commit after
    // this gate is therefore the actual provider publication below.
    let gate = ready.fixture.service.cleanup.hold(Phase::Committing);
    let state = ready.fixture.service.http_continuation.state.clone();
    let mut request = Box::pin(send(&ready, ready.headers.clone(), body(false, None)));
    let at_commit: Result<()> = match tokio::time::timeout(WAIT, async {
        tokio::select! {
            result = &mut request => { let _ = result?; anyhow::bail!("request returned before publication commit gate"); }
            () = gate.entered.notified() => Ok(()),
        }
    }).await {
        Ok(result) => result,
        Err(error) => Err(error.into()),
    };
    let held: Result<_> = (|| {
        at_commit?;
        let tickets = state
            .pending_tickets()
            .map_err(|_| anyhow::anyhow!("pending snapshot"))?;
        ensure!(
            tickets.len() == 1,
            "publication marker missing before commit"
        );
        let ticket = tickets.into_iter().next().context("publication ticket")?;
        ensure!(ready.fixture.service.cleanup.disposition() == Disposition::Pending);
        ensure!(state.matches(&ticket));
        Ok(ticket)
    })();
    // Drop only the external HTTP waiter. The actor and its lease stay owned.
    drop(request);
    let before_release = held.as_ref().is_ok_and(|ticket| state.matches(ticket));
    ready.fixture.service.cleanup.release_gate();
    let result: Result<()> = async {
        let ticket = held?;
        ensure!(before_release);
        server.wire().await?;
        tokio::time::timeout(WAIT, async {
            while state
                .pending_for(&ticket.execution)
                .map_err(|_| anyhow::anyhow!("pending owner"))?
            {
                tokio::task::yield_now().await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        tokio::time::timeout(WAIT, ready.fixture.service.admission.wait_drained()).await?;
        ensure!(
            row_count(&ready).await? == 0,
            "undelivered continuation remained active"
        );
        ensure!(ready.fixture.service.current().is_ok());
        let (status, bytes) = send(&ready, ready.headers.clone(), body(false, None)).await?;
        ensure!(status == StatusCode::OK);
        public_id(&bytes, false)?;
        server.wire().await?;
        ensure!(row_count(&ready).await? == 1);
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    server.close().await?;
    result
}

async fn admitted(ready: &Ready) -> Result<Arc<crate::tenancy::execution::TenantExecution>> {
    let submission = ready
        .fixture
        .service
        .submission(&ready.headers)
        .map_err(failure)?;
    ready
        .fixture
        .service
        .admit_execution(
            submission,
            MODEL,
            &bitrouter_ai::types::ApiProtocol::Responses,
        )
        .await
        .map_err(failure)
}

#[tokio::test]
async fn abandoned_lookup_keeps_read_permit_through_begin_and_rollback() -> Result<()> {
    use crate::tenancy::transaction::{Disposition, Phase};
    for phase in [Phase::Beginning, Phase::RollingBack] {
        let server = Server::start().await?;
        let ready = ready(&server).await?;
        ready
            .fixture
            .service
            .http_registry
            .join_reconciler_for_test()
            .await?;
        let execution = admitted(&ready).await?;
        let access = execution
            .http_access()
            .map_err(|code| anyhow::anyhow!("access {code:?}"))?;
        let gate = ready.fixture.service.cleanup.hold(phase);
        let port = ready.fixture.service.http_continuation.clone();
        let mut lookup = Box::pin(port.resolve(
            access,
            encode_gateway_continuation_id("absent-publication")?,
        ));
        let entered = tokio::time::timeout(WAIT, async {
            tokio::select! {
                result = &mut lookup => { let _ = result.map_err(|code| anyhow::anyhow!("lookup {code:?}"))?; anyhow::bail!("lookup returned before H gate"); }
                () = gate.entered.notified() => Ok::<_,anyhow::Error>(()),
            }
        }).await;
        let held = ready.fixture.service.reads.available_permits();
        drop(lookup);
        let after_drop = ready.fixture.service.reads.available_permits();
        ready.fixture.service.cleanup.release_gate();
        let result: Result<()> = async {
            entered??;
            ensure!(
                held == 7 && after_drop == 7,
                "cancelled caller released an actor-owned read permit"
            );
            tokio::time::timeout(WAIT, async {
                while ready.fixture.service.reads.available_permits() != 8 {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            ensure!(ready.fixture.service.cleanup.disposition() == Disposition::RolledBack);
            ensure!(ready.fixture.service.current().is_ok());
            ensure!(server.state.calls.load(Ordering::SeqCst) == 0);
            Ok(())
        }
        .await;
        drop(execution);
        ready.fixture.service.shutdown().await?;
        server.close().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn caller_revoke_while_provider_is_awaited_finishes_only_admitted_work() -> Result<()> {
    publication::held_caller_change(false).await
}

#[tokio::test]
async fn lookup_deadline_during_acknowledged_rollback_does_not_fence_service() -> Result<()> {
    use crate::tenancy::transaction::Phase;
    let server = Server::start().await?;
    let mut ready = ready(&server).await?;
    ready
        .fixture
        .service
        .execution_settings
        .execution_timeout_secs = Some(60);
    ready
        .fixture
        .service
        .http_registry
        .join_reconciler_for_test()
        .await?;
    let execution = admitted(&ready).await?;
    // Age the request before BEGIN. Expire it during rollback while leaving
    // H's independent cleanup window intact. The gate must not outlive that
    // window: such a timeout correctly fences even an eventually clean rollback.
    tokio::time::pause();
    tokio::time::advance(
        execution
            .remaining()
            .context("configured execution total")?
            .saturating_sub(crate::tenancy::transaction::CLEANUP_TIMEOUT / 2),
    )
    .await;
    tokio::time::resume();
    let access = execution
        .http_access()
        .map_err(|code| anyhow::anyhow!("access {code:?}"))?;
    let gate = ready.fixture.service.cleanup.hold(Phase::RollingBack);
    let rollback_count = ready.fixture.service.cleanup.sql_counts_for_test().1;
    let port = ready.fixture.service.http_continuation.clone();
    let mut lookup =
        Box::pin(port.resolve(access, encode_gateway_continuation_id("expired-read")?));
    let entered = tokio::time::timeout(WAIT, async {
        tokio::select! {
            result = &mut lookup => { let _ = result.map_err(|code| anyhow::anyhow!("lookup {code:?}"))?; anyhow::bail!("lookup returned before rollback gate"); }
            () = gate.entered.notified() => Ok::<_,anyhow::Error>(()),
        }
    }).await;
    if entered.as_ref().is_ok_and(|result| result.is_ok()) {
        tokio::time::pause();
        tokio::time::advance(
            execution
                .remaining()
                .context("configured execution total")?
                + Duration::from_millis(1),
        )
        .await;
        ready.fixture.service.cleanup.release_gate();
        tokio::time::resume();
    } else {
        ready.fixture.service.cleanup.release_gate();
    }
    let result: Result<()> = async {
        entered??;
        // The public waiter may have timed out before the owner confirms clean.
        ensure!(tokio::time::timeout(WAIT, &mut lookup).await?.is_err());
        tokio::time::timeout(WAIT, async {
            while ready.fixture.service.reads.available_permits() != 8 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        // Expiring the execution also schedules its no-send receipt. The
        // writer's latest disposition can already belong to that next job;
        // require the lookup's acknowledged rollback rather than its last phase.
        ensure!(ready.fixture.service.cleanup.sql_counts_for_test().1 == rollback_count + 1);
        ensure!(ready.fixture.service.current().is_ok());
        Ok(())
    }
    .await;
    drop(lookup);
    drop(execution);
    ready.fixture.service.shutdown().await?;
    server.close().await?;
    result
}

#[tokio::test]
async fn lost_actor_prepare_reply_is_compensated_after_caller_revocation() -> Result<()> {
    use crate::tenancy::{command::Command, transaction::Phase};
    use bitrouter_ai::tenancy::operation::Mutation;
    let mut server = Server::start().await?;
    let mut ready = ready(&server).await?;
    let credential = ready.fixture.issue("lost-reply-caller").await?;
    ready.headers = headers(
        &ready.fixture,
        credential.secret.as_deref().context("secret")?,
        &ready.revision.revision_id,
    )?;
    ready
        .fixture
        .service
        .http_registry
        .join_reconciler_for_test()
        .await?;
    ready
        .fixture
        .service
        .http_continuation
        .state
        .lose_next_prepare_reply();
    let gate = ready.fixture.service.cleanup.hold(Phase::Committing);
    let mut request = Box::pin(send(&ready, ready.headers.clone(), body(false, None)));
    let entered=tokio::time::timeout(WAIT,async {
        tokio::select! {
            result=&mut request=>{let _=result?;anyhow::bail!("request returned before real commit gate");}
            ()=gate.entered.notified()=>Ok::<_,anyhow::Error>(()),
        }
    }).await;
    let command = Command::Revoke(
        ready.fixture.tenant.clone(),
        credential.credential_id,
        Mutation {
            operation_id: "revoke-lost-reply-caller".try_into()?,
            expected_revision: credential.credential_revision,
            installation_id: ready.fixture.installation.clone(),
        },
    );
    let mut revoke = Box::pin(
        ready
            .fixture
            .service
            .execute(ready.fixture.operator.clone(), command),
    );
    let queued = futures::poll!(&mut revoke).is_pending();
    let markers = ready
        .fixture
        .service
        .http_continuation
        .state
        .pending_tickets();
    ready.fixture.service.cleanup.release_gate();
    let result: Result<()> = async {
        entered??;
        ensure!(queued);
        ensure!(markers.map_err(|_| anyhow::anyhow!("marker state"))?.len() == 1);
        tokio::time::timeout(WAIT, revoke).await?.map_err(failure)?;
        let (status, _) = tokio::time::timeout(WAIT, request).await??;
        ensure!(!status.is_success());
        server.wire().await?;
        ensure!(row_count(&ready).await? == 0);
        ensure!(
            ready
                .fixture
                .service
                .http_continuation
                .state
                .pending_tickets()
                .map_err(|_| anyhow::anyhow!("marker state"))?
                .is_empty()
        );
        ensure!(server.state.calls.load(Ordering::SeqCst) == 1);
        ensure!(ready.fixture.service.current().is_ok());
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    server.close().await?;
    result
}
