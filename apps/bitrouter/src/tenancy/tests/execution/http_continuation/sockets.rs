//! Real downstream disconnects with032 and034 sharing the same writer/custody.
use super::*;
use crate::tenancy::transaction::{Disposition, Phase};
use bitrouter_ai::tenancy::configuration::LearningMode;
use futures::FutureExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Ingress {
    address: std::net::SocketAddr,
    stop: CancellationToken,
    task: AbortOnDropHandle<std::io::Result<()>>,
}
impl Ingress {
    async fn start(ready: &Ready) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let app = Router::new()
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(axum::middleware::from_fn_with_state(
                ready.fixture.service.clone(),
                crate::tenancy::http::dispatch,
            ));
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
        }));
        Ok(Self {
            address,
            stop,
            task,
        })
    }
    async fn close(self) -> Result<()> {
        self.stop.cancel();
        tokio::time::timeout(WAIT, self.task).await???;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Wire {
    H1,
    #[cfg(feature = "http2")]
    H2,
}
fn wires() -> Vec<Wire> {
    #[cfg(feature = "http2")]
    {
        vec![Wire::H1, Wire::H2]
    }
    #[cfg(not(feature = "http2"))]
    {
        vec![Wire::H1]
    }
}
enum Client {
    H1(tokio::net::TcpStream),
    #[cfg(feature = "http2")]
    H2(Box<Http2>),
}
#[cfg(feature = "http2")]
struct Http2 {
    _sender: h2::client::SendRequest<axum::body::Bytes>,
    send: h2::SendStream<axum::body::Bytes>,
    response: Option<h2::client::ResponseFuture>,
    body: Option<h2::RecvStream>,
    driver: AbortOnDropHandle<std::result::Result<(), h2::Error>>,
}
impl Client {
    async fn start(wire: Wire, ingress: &Ingress, headers: &HeaderMap) -> Result<Self> {
        let bytes = serde_json::to_vec(&body(true, None))?;
        let mut socket = tokio::net::TcpStream::connect(ingress.address).await?;
        match wire {
            Wire::H1 => {
                // HTTP/1.1 message framing and the exact Content-Length body:
                // https://www.rfc-editor.org/rfc/rfc9112.html#section-2.1
                // https://www.rfc-editor.org/rfc/rfc9112.html#section-6.2
                let mut head = format!(
                    "POST /v1/responses HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    ingress.address,
                    bytes.len()
                );
                for (name, value) in headers {
                    head.push_str(&format!("{}: {}\r\n", name.as_str(), value.to_str()?));
                }
                head.push_str("\r\n");
                socket.write_all(head.as_bytes()).await?;
                socket.write_all(&bytes).await?;
                Ok(Self::H1(socket))
            }
            #[cfg(feature = "http2")]
            Wire::H2 => {
                // Drive the connection and release received stream capacity:
                // https://docs.rs/h2/latest/h2/client/index.html
                let (mut sender, connection) = h2::client::handshake(socket).await?;
                let driver = AbortOnDropHandle::new(tokio::spawn(connection));
                futures::future::poll_fn(|cx| sender.poll_ready(cx)).await?;
                let mut request =
                    Request::post(format!("http://{}/v1/responses", ingress.address)).body(())?;
                *request.headers_mut() = headers.clone();
                request
                    .headers_mut()
                    .insert("content-type", "application/json".parse()?);
                let (response, mut send) = sender.send_request(request, false)?;
                send.send_data(axum::body::Bytes::from(bytes), true)?;
                Ok(Self::H2(Box::new(Http2 {
                    _sender: sender,
                    send,
                    response: Some(response),
                    body: None,
                    driver,
                })))
            }
        }
    }
    async fn prefix(&mut self) -> Result<()> {
        tokio::time::timeout(WAIT, async {
            let mut seen = Vec::new();
            loop {
                match self {
                    Self::H1(socket) => {
                        let mut buffer = [0; 4096];
                        let count = socket.read(&mut buffer).await?;
                        ensure!(count != 0, "response ended before prefix");
                        seen.extend_from_slice(&buffer[..count]);
                    }
                    #[cfg(feature = "http2")]
                    Self::H2(client) => {
                        if let Some(response) = client.response.take() {
                            let response = response.await?;
                            ensure!(response.status() == StatusCode::OK);
                            client.body = Some(response.into_body());
                        }
                        let body = client.body.as_mut().context("h2 body")?;
                        let bytes = body.data().await.context("h2 prefix")??;
                        body.flow_control().release_capacity(bytes.len())?;
                        seen.extend_from_slice(&bytes);
                    }
                }
                ensure!(seen.len() <= 128 * 1024);
                let text = String::from_utf8_lossy(&seen);
                ensure!(!text.contains("resp-http-private"));
                ensure!(
                    !text.contains("response.completed"),
                    "terminal escaped gated provider"
                );
                if text.contains("owned reply") {
                    if matches!(self, Self::H1(_)) {
                        ensure!(
                            text.starts_with("HTTP/1.1 200 "),
                            "unexpected HTTP/1 status"
                        );
                    }
                    return Ok(());
                }
            }
        })
        .await?
    }
    async fn disconnect(self) -> Result<()> {
        match self {
            Self::H1(mut socket) => {
                socket.shutdown().await?;
                drop(socket);
            }
            #[cfg(feature = "http2")]
            Self::H2(mut client) => {
                client.send.send_reset(h2::Reason::CANCEL);
                client.driver.abort();
                match client.driver.await {
                    Err(error) if error.is_cancelled() => {}
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return Err(error.into()),
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }
}

struct Release(crate::tenancy::Service);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.cleanup.release_gate();
    }
}
async fn count(ready: &Ready, table: &str, predicate: &str) -> Result<i64> {
    Ok(ready
        .fixture
        .db
        .query_one(Statement::from_string(
            ready.fixture.db.get_database_backend(),
            format!("SELECT COUNT(*) AS n FROM {table} WHERE {predicate}"),
        ))
        .await?
        .context("count")?
        .try_get("", "n")?)
}
async fn closed(ready: &Ready, mode: LearningMode) -> Result<()> {
    ensure!(
        row_count(ready).await? == 0,
        "disconnected stream must not publish034 state"
    );
    ensure!(count(ready, "trajectory_requests", "1=1").await? == 1);
    ensure!(count(ready, "trajectory_requests", "status = 'started'").await? == 0);
    for table in [
        "trajectory_outbox",
        "eval_subjects",
        "eval_results",
        "eval_admission_events",
    ] {
        ensure!(
            count(ready, table, "1=1").await? == i64::from(mode != LearningMode::Off),
            "wrong {table} count"
        );
    }
    ensure!(count(ready, "trajectory_outbox", "delivered_at IS NULL").await? == 0);
    ready.fixture.service.cleanup.ensure_clean()?;
    Ok(())
}

#[tokio::test]
async fn real_http_partial_disconnect_keeps_trajectory_cleanup_without_continuation_publication()
-> Result<()> {
    for wire in wires() {
        for mode in [LearningMode::Off, LearningMode::Observe] {
            let mut provider = Server::start().await?;
            let tail = CancellationToken::new();
            let _release_tail = tail.clone().drop_guard();
            *provider
                .state
                .tail
                .lock()
                .map_err(|_| anyhow::anyhow!("tail"))? = Some(tail.clone());
            let ready = trajectory::guarded(&provider, mode).await?;
            let ingress = Ingress::start(&ready).await?;
            let mut client = Client::start(wire, &ingress, &ready.headers).await?;
            provider.wire().await?;
            client.prefix().await?;
            ensure!(count(&ready, "trajectory_requests", "status = 'started'").await? == 1);
            client.disconnect().await?;
            tokio::time::timeout(WAIT, ready.fixture.service.shutdown()).await??;
            tokio::time::timeout(WAIT, ready.fixture.service.admission.wait_drained()).await?;
            closed(&ready, mode).await?;
            ensure!(provider.state.calls.load(Ordering::SeqCst) == 1);
            tail.cancel();
            ingress.close().await?;
            provider.close().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn real_http_disconnect_while_publication_commit_is_held_keeps_both_producers_owned()
-> Result<()> {
    for wire in wires() {
        let mut provider = Server::start().await?;
        let tail = CancellationToken::new();
        let _release_tail = tail.clone().drop_guard();
        *provider
            .state
            .tail
            .lock()
            .map_err(|_| anyhow::anyhow!("tail"))? = Some(tail.clone());
        let ready = trajectory::guarded(&provider, LearningMode::Observe).await?;
        let ingress = Ingress::start(&ready).await?;
        let mut client = Client::start(wire, &ingress, &ready.headers).await?;
        provider.wire().await?;
        client.prefix().await?;
        ensure!(count(&ready, "trajectory_requests", "status = 'started'").await? == 1);
        // The032 start is acknowledged before the provider prefix. The existing
        // streaming pipeline prepares034 before terminal settlement; this gate
        // holds that publication commit. Both obligations must remain owned,
        // then034 compensates and032 settles after disconnect/acknowledgement.
        // The single writer never holds two SQL transactions at once.
        let gate = ready.fixture.service.cleanup.hold(Phase::Committing);
        let release = Release(ready.fixture.service.clone());
        tail.cancel();
        tokio::time::timeout(WAIT, gate.entered.notified()).await?;
        client.disconnect().await?;
        let mut shutdown = Box::pin(ready.fixture.service.shutdown());
        ensure!(
            tokio::time::timeout(Duration::from_millis(100), &mut shutdown)
                .await
                .is_err()
        );
        ensure!(ready.fixture.service.cleanup.disposition() == Disposition::Pending);
        ensure!(
            ready
                .fixture
                .service
                .http_continuation
                .state
                .pending_tickets()
                .map_err(|_| anyhow::anyhow!("publication marker"))?
                .len()
                == 1
        );
        ensure!(
            ready
                .fixture
                .service
                .admission
                .wait_drained()
                .now_or_never()
                .is_none()
        );
        ensure!(!ready.fixture.service.commands.is_closed());
        ensure!(!ready.fixture.service.http_continuation.state.barrier());
        drop(release);
        tokio::time::timeout(WAIT, &mut shutdown).await??;
        drop(shutdown);
        tokio::time::timeout(WAIT, ready.fixture.service.admission.wait_drained()).await?;
        closed(&ready, LearningMode::Observe).await?;
        ensure!(ready.fixture.service.http_continuation.state.barrier());
        ensure!(provider.state.calls.load(Ordering::SeqCst) == 1);
        ingress.close().await?;
        provider.close().await?;
    }
    Ok(())
}
