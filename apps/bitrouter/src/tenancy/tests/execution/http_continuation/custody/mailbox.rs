//! Real port admission behind the existing actor; no alternate writer or timer.
use super::*;
use crate::tenancy::{
    actor::Message,
    command::Command,
    store::StoredResponse,
    transaction::{Disposition, Phase},
};
use bitrouter_ai::tenancy::authority::PrincipalPut;
use tokio::sync::oneshot;

type Reply = oneshot::Receiver<Result<StoredResponse, crate::tenancy::error::Failure>>;

fn enqueue_principal(ready: &Ready, index: usize) -> Result<Reply> {
    let command = Command::Principal(
        format!("mailbox-principal-{index}").try_into()?,
        PrincipalPut {
            operation_id: format!("mailbox-create-{index}").try_into()?,
            expected_revision: None,
            issuer: "mailbox-fixture".into(),
            subject: format!("subject-{index}"),
            installation_id: ready.fixture.installation.clone(),
        },
    );
    command.validate()?;
    let (reply, receive) = oneshot::channel();
    ready
        .fixture
        .service
        .commands
        .try_send(
            Message {
                caller: ready.fixture.operator.clone(),
                command,
                reply,
            }
            .into(),
        )
        .map_err(|_| anyhow::anyhow!("bounded fixture enqueue"))?;
    Ok(receive)
}

async fn read_permits_returned(ready: &Ready) -> Result<()> {
    tokio::time::timeout(WAIT, async {
        while ready.fixture.service.reads.available_permits() != 8 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn full_actor_mailbox_rejects_http_lookup_without_leaking_its_read_permit() -> Result<()> {
    let mut provider = Server::start().await?;
    let ready = ready(&provider).await?;
    let outcome: Result<()> = async {
        let (status, bytes) = send(&ready, ready.headers.clone(), body(false, None)).await?;
        ensure!(status == StatusCode::OK);
        let public = public_id(&bytes, false)?;
        provider.wire().await?;
        let execution = admitted(&ready).await?;
        let access = execution
            .http_access()
            .map_err(|code| anyhow::anyhow!("access {code:?}"))?;
        let proof = &ready.fixture.service.cleanup;
        let gate = proof.hold(Phase::Active);
        let blocked = enqueue_principal(&ready, 0)?;
        let entered = tokio::time::timeout(WAIT, gate.entered.notified()).await;
        let mut queued = Vec::with_capacity(16);
        let checked: Result<()> = async {
            entered?;
            ensure!(ready.fixture.service.commands.capacity() == 16);
            // Finite, valid management commands occupy all ordinary slots. The
            // blocked writer retains its transaction; no read owns BEGIN yet.
            for index in 1..=16 {
                queued.push(enqueue_principal(&ready, index)?);
            }
            ensure!(ready.fixture.service.commands.capacity() == 0);
            ensure!(ready.fixture.service.reads.available_permits() == 8);
            let port = ready.fixture.service.http_continuation.clone();
            let mut lookup = Box::pin(port.resolve(access, public.clone()));
            ensure!(
                matches!(
                    futures::poll!(&mut lookup),
                    std::task::Poll::Ready(Err(ErrorCode::CapacityExceeded))
                ),
                "full mailbox must reject synchronously, without a waiter queue"
            );
            ensure!(ready.fixture.service.reads.available_permits() == 8);
            ensure!(proof.disposition() == Disposition::Pending);
            ensure!(provider.state.calls.load(Ordering::SeqCst) == 1);
            Ok(())
        }
        .await;
        proof.release_gate();
        tokio::time::timeout(WAIT, async {
            blocked.await?.map_err(failure)?;
            for reply in queued {
                reply.await?.map_err(failure)?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        drop(execution);
        checked?;
        ensure!(ready.fixture.service.current().is_ok());
        // A later actual resume succeeds with the original raw provider ID;
        // queue pressure neither consumed nor detached the retained mapping.
        let (status, bytes) =
            send(&ready, ready.headers.clone(), body(false, Some(&public))).await?;
        ensure!(status == StatusCode::OK);
        public_id(&bytes, false)?;
        ensure!(provider.wire().await?["previous_response_id"] == "resp-http-private-1");
        ensure!(row_count(&ready).await? == 2);
        read_permits_returned(&ready).await?;
        Ok(())
    }
    .await;
    ready.fixture.service.cleanup.release_gate();
    tokio::time::timeout(WAIT, ready.fixture.service.shutdown()).await??;
    provider.close().await?;
    outcome
}

#[tokio::test(flavor = "current_thread")]
async fn http_lookup_expiring_in_mailbox_never_begins_sql_and_keeps_service_healthy() -> Result<()>
{
    let mut provider = Server::start().await?;
    let ready = ready(&provider).await?;
    let outcome: Result<()> = async {
        let (status, bytes) = send(&ready, ready.headers.clone(), body(false, None)).await?;
        ensure!(status == StatusCode::OK);
        let public = public_id(&bytes, false)?;
        provider.wire().await?;
        let execution = admitted(&ready).await?;
        // Age only the admitted request, before starting the blocking writer.
        // H's cleanup clock has not started and is never advanced past its bound.
        tokio::time::pause();
        tokio::time::advance(
            execution
                .remaining()
                .saturating_sub(crate::tenancy::transaction::CLEANUP_TIMEOUT / 2),
        )
        .await;
        tokio::time::resume();
        let access = execution
            .http_access()
            .map_err(|code| anyhow::anyhow!("access {code:?}"))?;
        let proof = &ready.fixture.service.cleanup;
        let gate = proof.hold(Phase::Active);
        let blocked = enqueue_principal(&ready, 0)?;
        let entered = tokio::time::timeout(WAIT, gate.entered.notified()).await;
        let port = ready.fixture.service.http_continuation.clone();
        let mut lookup = Box::pin(port.resolve(access, public.clone()));
        let checked: Result<()> = async {
            entered?;
            ensure!(futures::poll!(&mut lookup).is_pending());
            ensure!(ready.fixture.service.commands.capacity() == 15);
            ensure!(ready.fixture.service.reads.available_permits() == 7);
            tokio::time::pause();
            tokio::time::advance(execution.remaining() + Duration::from_millis(1)).await;
            tokio::time::resume();
            ensure!(matches!(
                tokio::time::timeout(WAIT, &mut lookup).await?,
                Err(ErrorCode::AuthorityUnavailable)
            ));
            // Timing out the response cannot drop the queued job's permit.
            ensure!(ready.fixture.service.reads.available_permits() == 7);
            ensure!(proof.disposition() == Disposition::Pending);
            Ok(())
        }
        .await;
        drop(lookup);
        // No yield between releasing Active and installing Beginning on this
        // current-thread runtime: only the queued HTTP lookup can reach it.
        proof.release_gate();
        let unexpected_begin = proof.hold(Phase::Beginning);
        let settled: Result<()> = async {
            tokio::time::timeout(WAIT, blocked)
                .await??
                .map_err(failure)?;
            tokio::select! {
                biased;
                () = unexpected_begin.entered.notified() => {
                    anyhow::bail!("an expired mailbox read entered BEGIN");
                }
                returned = read_permits_returned(&ready) => returned?,
            }
            ensure!(proof.disposition() == Disposition::NoTransaction);
            Ok(())
        }
        .await;
        proof.release_gate();
        drop(execution);
        checked?;
        settled?;
        ensure!(ready.fixture.service.current().is_ok());
        ensure!(provider.state.calls.load(Ordering::SeqCst) == 1);
        ensure!(row_count(&ready).await? == 1);
        let (status, bytes) =
            send(&ready, ready.headers.clone(), body(false, Some(&public))).await?;
        ensure!(status == StatusCode::OK);
        public_id(&bytes, false)?;
        ensure!(provider.wire().await?["previous_response_id"] == "resp-http-private-1");
        ensure!(row_count(&ready).await? == 2);
        Ok(())
    }
    .await;
    ready.fixture.service.cleanup.release_gate();
    tokio::time::timeout(WAIT, ready.fixture.service.shutdown()).await??;
    provider.close().await?;
    outcome
}
