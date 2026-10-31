//! Actual ingress sockets use production dispatch and required trajectory custody.
use super::*;
use bitrouter_ai::tenancy::{authority::CredentialIssued, operation::DispatchState};
use futures::FutureExt;

mod client;
mod server;
use client::{Client, Protocol, protocols};
use server::{Listener, Provider};

#[derive(Clone, Copy)]
enum Disconnect {
    BeforeHeaders,
    DuringStream,
}

async fn issue(
    fixture: &Fixture,
    revision: &RevisionId,
) -> Result<(CredentialIssued, ::http::HeaderMap)> {
    let issued = fixture.issue("socket-execution").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        fixture,
        issued.secret.as_deref().context("secret")?,
        revision,
    )?;
    Ok((issued, headers))
}
async fn revoke(fixture: &Fixture, issued: CredentialIssued) -> Result<()> {
    fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Revoke(
                fixture.tenant.clone(),
                issued.credential_id,
                Mutation {
                    operation_id: "socket-revoke".try_into()?,
                    expected_revision: issued.credential_revision,
                    installation_id: fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    Ok(())
}
async fn closure(fixture: &Fixture, expected: i64) -> Result<()> {
    ensure!(count(&fixture.db, "trajectory_requests", "1=1").await? == expected);
    ensure!(count(&fixture.db, "trajectory_requests", "status = 'started'").await? == 0);
    ensure!(count(&fixture.db, "trajectory_outbox", "delivered_at IS NULL").await? == 0);
    for table in [
        "trajectory_outbox",
        "eval_subjects",
        "eval_results",
        "eval_admission_events",
    ] {
        ensure!(count(&fixture.db, table, "1=1").await? == expected);
    }
    Ok(())
}
async fn disconnect(protocol: Protocol, when: Disconnect, revoked: bool) -> Result<()> {
    let provider = Provider::new();
    let _headers_cleanup = provider.headers.clone().drop_guard();
    let _tail_cleanup = provider.tail.clone().drop_guard();
    let upstream = provider.start().await?;
    let (fixture, revision) = fixture(
        &format!("http://{}/v1", upstream.address),
        LearningMode::Observe,
    )
    .await?;
    let (issued, headers) = issue(&fixture, &revision).await?;
    let ingress = Listener::start(router(&fixture)).await?;
    let mut client = Client::start(protocol, ingress.address, &headers).await?;
    tokio::time::timeout(TIMEOUT, provider.received.notified()).await?;
    ensure!(provider.calls.load(Ordering::SeqCst) == 1);
    ensure!(count(&fixture.db, "trajectory_requests", "status = 'started'").await? == 1);
    ensure!(count(&fixture.db, "trajectory_outbox", "1=1").await? == 0);
    if revoked {
        revoke(&fixture, issued).await?;
    }
    if matches!(when, Disconnect::DuringStream) {
        provider.headers.cancel();
        client.prefix().await?;
    }
    // Cancellation closes provider I/O. Hold the actual terminal transaction,
    // rather than relying on withheld provider headers to retain cleanup.
    let gate = fixture
        .service
        .cleanup
        .hold(crate::tenancy::transaction::Phase::Committing);
    let _release = ReleaseGate(fixture.service.clone());
    client.disconnect().await?;
    let mut shutdown = Box::pin(fixture.service.shutdown());
    ensure!(futures::poll!(&mut shutdown).is_pending());
    tokio::time::timeout(TIMEOUT, gate.entered.notified())
        .await
        .context("cancelled provider did not reach terminal COMMIT")?;
    ensure!(
        fixture.service.cleanup.disposition() == crate::tenancy::transaction::Disposition::Pending
    );
    ensure!(
        tokio::time::timeout(Duration::from_millis(100), &mut shutdown)
            .await
            .is_err()
    );
    ensure!(
        fixture
            .service
            .admission
            .wait_drained()
            .now_or_never()
            .is_none()
    );
    fixture.service.cleanup.release_gate();
    tokio::time::timeout(TIMEOUT, &mut shutdown).await??;
    tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
    fixture.service.pipelines.close_and_drain().await?;
    closure(&fixture, 1).await?;
    let receipt = only_receipt(&fixture).await?;
    ensure!(receipt.outcome != bitrouter_ai::tenancy::execution::Outcome::Completed);
    ensure!(receipt.attempts.len() == 1);
    if matches!(when, Disconnect::BeforeHeaders) {
        ensure!(
            receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Unknown
                && receipt.attempts[0].outcome
                    == bitrouter_ai::tenancy::execution::Outcome::Unknown
        );
    }
    ensure!(provider.calls.load(Ordering::SeqCst) == 1);
    // Neither withheld headers nor the next stream chunk is needed to settle.
    ensure!(!provider.tail.is_cancelled());
    if matches!(when, Disconnect::BeforeHeaders) {
        ensure!(!provider.headers.is_cancelled());
        // The remote handler may outlive the closed socket. Release its fixture
        // gates only after proving local receipt completion and capacity drain.
        provider.headers.cancel();
        provider.tail.cancel();
    }
    tokio::time::timeout(TIMEOUT, provider.dropped.cancelled()).await?;
    ingress.close().await?;
    upstream.close().await?;
    Ok(())
}

#[tokio::test]
async fn real_sockets_disconnect_before_headers_and_during_stream_keep_owned_closure() -> Result<()>
{
    for protocol in protocols() {
        for when in [Disconnect::BeforeHeaders, Disconnect::DuringStream] {
            disconnect(protocol, when, false).await?;
        }
    }
    Ok(())
}
#[tokio::test]
async fn real_sockets_revocation_after_committed_begin_still_allows_terminal_cleanup() -> Result<()>
{
    for protocol in protocols() {
        disconnect(protocol, Disconnect::BeforeHeaders, true).await?;
    }
    Ok(())
}

#[tokio::test]
async fn real_sockets_revoke_queued_execution_before_begin_has_no_provider_or_storage_effects()
-> Result<()> {
    for protocol in protocols() {
        let provider = Provider::new();
        let _headers_cleanup = provider.headers.clone().drop_guard();
        let _tail_cleanup = provider.tail.clone().drop_guard();
        let upstream = provider.start().await?;
        let (fixture, revision) = fixture(
            &format!("http://{}/v1", upstream.address),
            LearningMode::Observe,
        )
        .await?;
        let document = fixture
            .service
            .configuration(&fixture.operator, &fixture.tenant, &revision)
            .await
            .map_err(failure)?;
        let mut input = fixture.configuration_input("socket-one-active")?;
        input.document = document.document;
        input.document.limits.active_executions = 1;
        input.document.limits.queued_work = 1;
        let revision = match fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Configuration(fixture.tenant.clone(), input),
            )
            .await
            .map_err(failure)?
        {
            StoredResponse::Configuration(row) => row.revision_id,
            _ => anyhow::bail!("expected configuration"),
        };
        let (issued, headers) = issue(&fixture, &revision).await?;
        let held = admitted(&fixture, &headers).await?;
        let ingress = Listener::start(router(&fixture)).await?;
        let mut client = Client::start(protocol, ingress.address, &headers).await?;
        tokio::time::timeout(TIMEOUT, async {
            while fixture.service.admission.queued_for_test() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        revoke(&fixture, issued).await?;
        drop(held);
        let error = client.rejection().await?;
        ensure!(
            error.code == ErrorCode::Unauthorized
                && error.dispatch_state == DispatchState::NotStarted
        );
        client.disconnect().await?;
        fixture.service.shutdown().await?;
        ensure!(provider.calls.load(Ordering::SeqCst) == 0);
        closure(&fixture, 0).await?;
        ingress.close().await?;
        upstream.close().await?;
    }
    Ok(())
}

struct ReleaseGate(Service);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.cleanup.release_gate();
    }
}
struct Paused;
impl Paused {
    fn new() -> Self {
        tokio::time::pause();
        Self
    }
}
impl Drop for Paused {
    fn drop(&mut self) {
        tokio::time::resume();
    }
}

#[tokio::test]
async fn real_socket_begin_deadline_and_shutdown_wait_for_rollback_acknowledgment() -> Result<()> {
    use crate::tenancy::transaction::Phase;
    for protocol in protocols() {
        let provider = Provider::new();
        let _headers_cleanup = provider.headers.clone().drop_guard();
        let _tail_cleanup = provider.tail.clone().drop_guard();
        let upstream = provider.start().await?;
        let (fixture, revision) = fixture(
            &format!("http://{}/v1", upstream.address),
            LearningMode::Observe,
        )
        .await?;
        let (_, headers) = issue(&fixture, &revision).await?;
        let ingress = Listener::start(router(&fixture)).await?;
        let rollback_count = fixture.service.cleanup.sql_counts_for_test().1;
        let gate = fixture.service.cleanup.hold(Phase::Active);
        let _release = ReleaseGate(fixture.service.clone());
        let client = Client::start(protocol, ingress.address, &headers).await?;
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        client.disconnect().await?;
        let mut shutdown = Box::pin(fixture.service.shutdown());
        ensure!(futures::poll!(&mut shutdown).is_pending());
        let paused = Paused::new();
        tokio::time::advance(crate::tenancy::eval::WORK + Duration::from_millis(1)).await;
        tokio::time::timeout(
            Duration::from_secs(1),
            fixture.service.cleanup.read_stopped.notified(),
        )
        .await?;
        ensure!(futures::poll!(&mut shutdown).is_pending());
        ensure!(
            fixture
                .service
                .admission
                .wait_drained()
                .now_or_never()
                .is_none()
        );
        ensure!(provider.calls.load(Ordering::SeqCst) == 0);
        fixture.service.cleanup.release_gate();
        drop(paused);
        // Join the same shutdown future after releasing the actual BEGIN gate.
        // A later receipt COMMIT/producer-close can replace last-job disposition,
        // but cannot erase this fixture's one acknowledged rollback.
        tokio::time::timeout(TIMEOUT, &mut shutdown).await??;
        ensure!(fixture.service.cleanup.sql_counts_for_test().1 == rollback_count + 1);
        let receipt = only_receipt(&fixture).await?;
        ensure!(receipt.attempts.is_empty());
        ensure!(receipt.outcome != bitrouter_ai::tenancy::execution::Outcome::Completed);
        fixture.service.cleanup.ensure_clean()?;
        ensure!(fixture.service.eval.healthy());
        closure(&fixture, 0).await?;
        ensure!(provider.calls.load(Ordering::SeqCst) == 0);
        ingress.close().await?;
        upstream.close().await?;
    }
    Ok(())
}
