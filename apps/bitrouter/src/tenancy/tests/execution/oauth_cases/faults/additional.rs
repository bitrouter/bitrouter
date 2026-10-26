//! Additional live OAuth competition and broken-body coverage.
use super::*;

/// Release every fixture gate even when an assertion or bounded wait fails.
/// Shutdown and both listener joins are attempted before returning any error.
async fn finish_additional_oauth_case(mut harness: Harness, checked: Result<()>) -> Result<()> {
    harness.fixture.service.cleanup.release_gate();
    harness.provider.refresh_gate.close();
    harness.provider.model_gate.close();
    let shutdown = tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown())
        .await
        .context("additional OAuth case shutdown deadline")
        .and_then(|result| result);
    harness.gateway_stop.cancel();
    harness.provider_stop.cancel();
    let gateway = async {
        tokio::time::timeout(TIMEOUT, &mut harness._gateway).await???;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let provider = async {
        tokio::time::timeout(TIMEOUT, &mut harness._provider).await???;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let mut failures = Vec::new();
    for (label, result) in [
        ("case", checked),
        ("shutdown", shutdown),
        ("gateway", gateway),
        ("provider", provider),
    ] {
        if let Err(error) = result {
            failures.push(format!("{label}: {error:#}"));
        }
    }
    ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

async fn saved_refresh_fact(
    harness: &Harness,
    id: &ExecutionId,
) -> Result<crate::tenancy::receipts::auxiliary::Fact> {
    use sea_orm::sea_query::{Alias, Expr, Query};
    // Point lookup verifies the public/private immutable pair before inspecting it.
    harness.receipt(id).await?;
    let query = Query::select()
        .column(Alias::new("evidence_json"))
        .from(Alias::new("tenancy_receipt_auxiliary"))
        .and_where(
            Expr::col(Alias::new("installation_id")).eq(harness.fixture.installation.as_str()),
        )
        .and_where(Expr::col(Alias::new("tenant_id")).eq(harness.fixture.tenant.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(id.as_str()))
        .to_owned();
    let row = harness
        .fixture
        .db
        .query_one(harness.fixture.db.get_database_backend().build(&query))
        .await?
        .context("saved refresh evidence")?;
    let facts: Vec<crate::tenancy::receipts::auxiliary::Fact> =
        serde_json::from_str(&row.try_get::<String>("", "evidence_json")?)?;
    ensure!(facts.len() == 1, "one refresh obligation per execution");
    facts.into_iter().next().context("refresh fact")
}

/// Both HTTP consumers pass the real generation-zero read. Their Start jobs
/// compete at the ordinary writer, so the loser must take a confirmed rollback.
#[tokio::test]
async fn simultaneous_same_slot_start_cas_posts_once_and_preserves_the_loser_receipt() -> Result<()>
{
    use crate::tenancy::receipts::auxiliary::Outcome as AuxiliaryOutcome;
    use bitrouter_ai::tenancy::operation::{DispatchState, ErrorCode};
    for &h2 in modes() {
        let mut harness = Harness::new(true).await?;
        let checked = tokio::time::timeout(TIMEOUT, async {
            let read = harness
                .fixture
                .service
                .hold_receipt_credential_read_for_test()?;
            let _release_read = read.release.clone().drop_guard();
            let client = Harness::client(h2)?;
            let version = if h2 {
                reqwest::Version::HTTP_2
            } else {
                reqwest::Version::HTTP_11
            };
            let request = harness.send(&client, false);
            let loser = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
            read.entered.notified().await;
            let loser_id = read
                .execution
                .get()
                .context("loser completed the actual credential read")?
                .clone();
            let (_, rollbacks_before) = harness.fixture.service.cleanup.sql_counts_for_test();
            let start = harness.fixture.service.cleanup.hold(Phase::Committing);
            let request = harness.send(&client, false);
            let winner = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
            start.entered.notified().await;
            ensure!(harness.provider.requests.load(Ordering::SeqCst) == 0);
            // The actor has performed the winner's Start DML but has not committed
            // or published it. The first consumer can still validate its read.
            let free = harness.fixture.service.completions.capacity();
            ensure!(free > 0);
            read.release.cancel();
            while harness.fixture.service.completions.capacity() == free {
                tokio::task::yield_now().await;
            }
            ensure!(
                harness.fixture.service.completions.capacity() == free - 1,
                "the second Start must be queued before releasing the first COMMIT"
            );
            ensure!(!loser.is_finished() && !winner.is_finished());
            harness.fixture.service.cleanup.release_gate();

            harness
                .refresh_seen
                .recv()
                .await
                .context("winner's actual token POST")?;
            let response = loser.await??;
            ensure!(response.status() == StatusCode::TOO_MANY_REQUESTS);
            ensure!(response.version() == version);
            ensure!(execution_id(&response)? == loser_id);
            let error: bitrouter_ai::tenancy::operation::Error = response.json().await?;
            ensure!(
                error.code == ErrorCode::CapacityExceeded
                    && error.dispatch_state == DispatchState::NotStarted
            );
            let (_, rollbacks_after) = harness.fixture.service.cleanup.sql_counts_for_test();
            ensure!(
                rollbacks_after > rollbacks_before,
                "loser must reach the actor's row-CAS rollback"
            );
            let loser_receipt = harness.receipt(&loser_id).await?;
            let loser_bytes = serde_json::to_vec(&loser_receipt)?;
            ensure!(loser_receipt.attempts.is_empty() && loser_receipt.outcome == Outcome::Failed);
            ensure!(
                loser_receipt.usage.input_tokens.is_none()
                    && loser_receipt.usage.output_tokens.is_none()
            );
            let lost = saved_refresh_fact(&harness, &loser_id).await?;
            ensure!(lost.outcome == AuxiliaryOutcome::NotSent && lost.generation == 0);
            ensure!(lost.request_digest.is_none() && lost.http_status.is_none());
            ensure!(
                harness.fixture.service.current().is_ok()
                    && harness.fixture.service.admission.receipts_ready()
            );
            ensure!(
                harness.provider.refreshes.load(Ordering::SeqCst) == 1
                    && harness.provider.models.load(Ordering::SeqCst) == 0
            );

            harness.provider.refresh_gate.add_permits(1);
            harness.provider.model_gate.add_permits(2);
            let response = winner.await??;
            ensure!(response.status() == StatusCode::OK);
            ensure!(response.version() == version);
            let winner_id = execution_id(&response)?;
            response.bytes().await?;
            ensure!(winner_id != loser_id);
            let won = saved_refresh_fact(&harness, &winner_id).await?;
            ensure!(
                won.outcome == AuxiliaryOutcome::Refreshed
                    && won.generation == 0
                    && won.http_status == Some(200)
            );
            ensure!(
                won.work != lost.work && won.slot == lost.slot && won.credential == lost.credential
            );
            ensure!(harness.receipt(&winner_id).await?.attempts.len() == 1);
            let row = harness.row().await?;
            ensure!(
                row.metadata.generation == 1 && matches!(row.metadata.state, storage::State::Ready)
            );
            ensure!(
                row.token(&harness.resources.oauth_crypto()?)?.access_token == "rotated-access"
            );

            // The conflict is local to the loser: the same ingress credential and
            // committed generation still serve another real request without refresh.
            let response = harness.send(&client, false).send().await?;
            ensure!(response.status() == StatusCode::OK);
            let next_id = execution_id(&response)?;
            response.bytes().await?;
            ensure!(harness.receipt(&next_id).await?.attempts.len() == 1);
            ensure!(
                harness.provider.refreshes.load(Ordering::SeqCst) == 1
                    && harness.provider.models.load(Ordering::SeqCst) == 2
            );
            ensure!(harness.provider.requests.load(Ordering::SeqCst) == 3);
            ensure!(serde_json::to_vec(&harness.receipt(&loser_id).await?)? == loser_bytes);
            harness.fixture.service.admission.wait_drained().await;
            let inventory = crate::tenancy::receipts::store::inventory(
                &harness.fixture.db,
                &harness.fixture.installation,
            )
            .await?;
            ensure!(inventory.rows == 3 && inventory.unknown.is_empty());
            Ok(())
        })
        .await
        .context("same-slot Start CAS case deadline")
        .and_then(|result| result);
        finish_additional_oauth_case(harness, checked).await?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BrokenRefreshBody {
    Truncated,
    Stalled,
}

/// Raw HTTP gives a real Content-Length violation without a server framework
/// replacing an errored body before its response headers reach the socket.
struct RejectedRefreshPeer {
    endpoint: String,
    requests: Arc<AtomicUsize>,
    written: mpsc::Receiver<()>,
    disconnected: mpsc::Receiver<()>,
    stop: tokio_util::sync::CancellationToken,
    task: AbortOnDropHandle<Result<()>>,
}
impl RejectedRefreshPeer {
    async fn start(status: StatusCode, body: BrokenRefreshBody) -> Result<Self> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/oauth/token", listener.local_addr()?);
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let stop = tokio_util::sync::CancellationToken::new();
        let stopping = stop.clone();
        let (written_tx, written) = mpsc::channel(8);
        let (disconnected_tx, disconnected) = mpsc::channel(8);
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                let (mut socket, _) = tokio::select! {
                    () = stopping.cancelled() => break,
                    accepted = listener.accept() => accepted?,
                };
                observed.fetch_add(1, Ordering::SeqCst);
                tokio::select! {
                    () = stopping.cancelled() => break,
                    request = tokio::time::timeout(TIMEOUT, read_refresh_form(&mut socket)) => request??,
                }
                let response = format!(
                    "HTTP/1.1 {} Rejected\r\nContent-Type: application/json\r\nContent-Length: 256\r\nConnection: close\r\n\r\n{{\"error\":",
                    status.as_u16()
                );
                tokio::select! {
                    () = stopping.cancelled() => break,
                    written = tokio::time::timeout(TIMEOUT, async {
                        socket.write_all(response.as_bytes()).await?;
                        socket.flush().await
                    }) => written??,
                }
                written_tx
                    .try_send(())
                    .context("refresh header observation capacity")?;
                if body == BrokenRefreshBody::Stalled {
                    // Keep the connection and incomplete body alive until the
                    // production refresher's own 30-second timeout closes it.
                    let mut byte = [0_u8; 1];
                    let read = tokio::select! {
                        () = stopping.cancelled() => break,
                        read = tokio::time::timeout(Duration::from_secs(40), socket.read(&mut byte)) => read?,
                    };
                    match read {
                        Ok(0) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                        Ok(_) => {
                            anyhow::bail!("unexpected request bytes while refresh body is stalled")
                        }
                        Err(error) => return Err(error.into()),
                    }
                } else {
                    tokio::time::timeout(TIMEOUT, socket.shutdown()).await??;
                }
                disconnected_tx
                    .try_send(())
                    .context("refresh disconnect observation capacity")?;
            }
            Ok(())
        }));
        Ok(Self {
            endpoint,
            requests,
            written,
            disconnected,
            stop,
            task,
        })
    }
    async fn finish(mut self) -> Result<()> {
        self.stop.cancel();
        tokio::time::timeout(TIMEOUT, &mut self.task).await???;
        Ok(())
    }
}
impl Drop for RejectedRefreshPeer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn read_refresh_form(socket: &mut tokio::net::TcpStream) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    let (header_end, length) = loop {
        let mut chunk = [0_u8; 1024];
        let count = socket.read(&mut chunk).await?;
        ensure!(
            count != 0 && bytes.len() + count <= 8192,
            "refresh request header bound"
        );
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = std::str::from_utf8(&bytes[..index])?;
            let mut lines = head.split("\r\n");
            ensure!(lines.next() == Some("POST /oauth/token HTTP/1.1"));
            let mut length = None;
            for line in lines {
                let (name, value) = line.split_once(':').context("HTTP header")?;
                ensure!(!name.eq_ignore_ascii_case("transfer-encoding"));
                if name.eq_ignore_ascii_case("content-length") {
                    ensure!(length.is_none(), "duplicate request Content-Length");
                    length = Some(value.trim().parse::<usize>()?);
                }
            }
            let length = length.context("refresh form Content-Length")?;
            ensure!(
                index + 4 <= 4096 && length <= 4096,
                "refresh form byte bound"
            );
            break (index + 4, length);
        }
    };
    while bytes.len() < header_end + length {
        let mut chunk = [0_u8; 1024];
        let count = socket.read(&mut chunk).await?;
        ensure!(
            count != 0 && bytes.len() + count <= 8192,
            "refresh form body bound"
        );
        bytes.extend_from_slice(&chunk[..count]);
    }
    ensure!(
        bytes.len() == header_end + length,
        "unexpected bytes after refresh form"
    );
    let form: Vec<_> = url::form_urlencoded::parse(&bytes[header_end..]).collect();
    ensure!(form.len() == 3);
    let registration =
        crate::gateway::providers::login::find("anthropic").context("registration")?;
    for (name, expected) in [
        ("grant_type", "refresh_token"),
        ("refresh_token", "seed-refresh"),
        ("client_id", registration.auth.client_id.as_str()),
    ] {
        ensure!(
            form.iter()
                .filter(|(key, value)| key.as_ref() == name && value.as_ref() == expected)
                .count()
                == 1,
            "refresh must use the exact retained form"
        );
    }
    Ok(())
}

async fn rejected_refresh_body_case(
    body: BrokenRefreshBody,
    status: StatusCode,
    h2: bool,
) -> Result<()> {
    use crate::tenancy::receipts::{auxiliary::Outcome as AuxiliaryOutcome, store as receipts};
    use bitrouter_ai::tenancy::execution::ReceiptState;
    let mut peer = RejectedRefreshPeer::start(status, body).await?;
    let created = Harness::with_token_endpoint(true, false, None, Some(&peer.endpoint)).await;
    let mut harness = match created {
        Ok(harness) => harness,
        Err(error) => {
            let closed = peer.finish().await;
            return Err(anyhow::anyhow!(
                "OAuth fixture setup: {error:#}; raw peer cleanup: {closed:?}"
            ));
        }
    };
    let checked = tokio::time::timeout(Duration::from_secs(40), async {
        let client = Harness::client(h2)?;
        // Ingress stays connected beyond the real refresh timeout. No production
        // deadline or paused runtime clock is changed by this fault fixture.
        let request = harness
            .send(&client, false)
            .timeout(Duration::from_secs(40));
        let caller = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
        peer.written
            .recv()
            .await
            .context("actual rejection headers and partial body")?;
        let body_wait = tokio::time::Instant::now();
        if body == BrokenRefreshBody::Stalled {
            ensure!(
                !caller.is_finished(),
                "stalled diagnostic body must still be owned"
            );
        }
        let response = caller.await??;
        ensure!(!response.status().is_success());
        ensure!(
            response.version()
                == if h2 {
                    reqwest::Version::HTTP_2
                } else {
                    reqwest::Version::HTTP_11
                }
        );
        if body == BrokenRefreshBody::Stalled {
            ensure!(
                body_wait.elapsed() >= Duration::from_secs(25),
                "the incomplete body must reach the real refresh timeout"
            );
        }
        let id = execution_id(&response)?;
        response.bytes().await?;
        peer.disconnected
            .recv()
            .await
            .context("actual rejected refresh socket closed")?;
        ensure!(peer.requests.load(Ordering::SeqCst) == 1);
        ensure!(
            harness.provider.requests.load(Ordering::SeqCst) == 0
                && harness.provider.models.load(Ordering::SeqCst) == 0
        );
        let receipt = harness.receipt(&id).await?;
        let receipt_bytes = serde_json::to_vec(&receipt)?;
        ensure!(receipt.state == ReceiptState::Incomplete && receipt.outcome == Outcome::Failed);
        ensure!(
            receipt.attempts.is_empty()
                && receipt.usage.input_tokens.is_none()
                && receipt.usage.output_tokens.is_none()
        );
        let fact = saved_refresh_fact(&harness, &id).await?;
        ensure!(
            fact.outcome == AuxiliaryOutcome::Rejected && fact.http_status == Some(status.as_u16())
        );
        ensure!(
            fact.request_digest.is_some()
                && fact.generation == 0
                && fact.credential == harness.slot
        );
        let slot = harness.row().await?;
        let storage::State::Quarantined(work) = &slot.metadata.state else {
            anyhow::bail!("consumed rejected refresh must quarantine its original seed");
        };
        ensure!(work.execution == id && work.id == fact.work && slot.metadata.generation == 0);
        let before =
            receipts::inventory(&harness.fixture.db, &harness.fixture.installation).await?;
        ensure!(before.rows == 1 && before.unknown.is_empty());
        ensure!(
            storage::pending_unknown(&harness.fixture.db, &harness.fixture.installation)
                .await?
                .is_empty()
        );
        harness.fixture.service.admission.wait_drained().await;
        harness.reopen().await?;
        let after = receipts::inventory(&harness.fixture.db, &harness.fixture.installation).await?;
        ensure!(
            after.rows == before.rows && after.bytes == before.bytes && after.unknown.is_empty()
        );
        ensure!(slot.same(&harness.row().await?));
        ensure!(serde_json::to_vec(&harness.receipt(&id).await?)? == receipt_bytes);
        let response = harness.send(&client, false).send().await?;
        ensure!(!response.status().is_success());
        response.bytes().await?;
        ensure!(
            peer.requests.load(Ordering::SeqCst) == 1
                && harness.provider.requests.load(Ordering::SeqCst) == 0,
            "restart cannot reuse the rejected seed or fall through to a model"
        );
        ensure!(slot.same(&harness.row().await?));
        ensure!(
            receipts::inventory(&harness.fixture.db, &harness.fixture.installation)
                .await?
                .unknown
                .is_empty()
        );
        Ok(())
    })
    .await
    .context("rejected refresh body case deadline")
    .and_then(|result| result);
    // Unblock the raw peer on every outcome before draining detached execution.
    peer.stop.cancel();
    let closed = peer.finish().await;
    let checked = match (checked, closed) {
        (Ok(()), Ok(())) => Ok(()),
        (checked, closed) => Err(anyhow::anyhow!(
            "rejection case: {checked:?}; raw peer: {closed:?}"
        )),
    };
    finish_additional_oauth_case(harness, checked).await
}

/// HTTP401 is observed before a genuine fixed-length body truncation.
#[tokio::test]
async fn rejected_refresh_truncated_body_keeps_status_and_restart_accounting() -> Result<()> {
    for &h2 in modes() {
        rejected_refresh_body_case(BrokenRefreshBody::Truncated, StatusCode::UNAUTHORIZED, h2)
            .await?;
    }
    Ok(())
}

/// Uses approximately 30 seconds of real time to exercise the actual refresher
/// timeout after HTTP403 headers, while the original ingress waiter stays live.
#[tokio::test]
async fn rejected_refresh_stalled_body_keeps_status_after_actual_timeout() -> Result<()> {
    rejected_refresh_body_case(BrokenRefreshBody::Stalled, StatusCode::FORBIDDEN, false).await
}
