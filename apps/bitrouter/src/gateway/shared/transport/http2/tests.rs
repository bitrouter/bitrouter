use super::*;

mod compression;
mod regressions;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::ensure;
use axum::routing::post;
use tokio::net::TcpStream;
use tokio::sync::{Barrier, Notify};
use tokio::task::JoinHandle;

use super::super::owned::Transport;

fn limits() -> Limits {
    Limits {
        connections: 8,
        requests: 128,
        write_timeout: Duration::from_millis(200),
        request_timeout: Duration::from_millis(200),
        ..super::super::tests::limits()
    }
}

async fn start(router: Router, limits: Limits) -> Result<(Transport, SocketAddr)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    Ok((Transport::start(listener, router, limits)?, address))
}

struct Client {
    sender: h2::client::SendRequest<Bytes>,
    driver: JoinHandle<Result<(), h2::Error>>,
}

impl Client {
    async fn connect(address: SocketAddr) -> Result<Self> {
        let (sender, connection) =
            h2::client::handshake(TcpStream::connect(address).await?).await?;
        Ok(Self {
            sender,
            driver: tokio::spawn(connection),
        })
    }

    async fn request(
        &mut self,
        path: &str,
        end: bool,
    ) -> Result<(h2::client::ResponseFuture, SendStream<Bytes>)> {
        poll_fn(|cx| self.sender.poll_ready(cx)).await?;
        Ok(self.sender.send_request(
            Request::builder()
                .method("POST")
                .uri(format!("http://fixture{path}"))
                .body(())?,
            end,
        )?)
    }

    async fn close(self) -> Result<()> {
        self.driver.abort();
        ensure!(self.driver.await.is_err());
        Ok(())
    }
}

async fn collect(mut body: h2::RecvStream) -> Result<Bytes> {
    let mut bytes = Vec::new();
    while let Some(data) = body.data().await {
        let data = data?;
        body.flow_control().release_capacity(data.len())?;
        bytes.extend_from_slice(&data);
    }
    Ok(bytes.into())
}

async fn stop(owner: Transport) -> Result<()> {
    let report = owner
        .drain_until(Instant::now() + Duration::from_secs(5))
        .await?;
    ensure!(report.is_clean(), "unclean transport drain: {report:?}");
    Ok(())
}

#[tokio::test]
async fn same_listener_serves_http1_and_128_concurrent_http2_streams() -> Result<()> {
    timeout(Duration::from_secs(15), async {
        let barrier = Arc::new(Barrier::new(128));
        let router = Router::new()
            .route("/echo", post(|body: Bytes| async move { body }))
            .route(
                "/sse",
                post(move || {
                    let barrier = barrier.clone();
                    async move {
                        barrier.wait().await;
                        (
                            [(header::CONTENT_TYPE, "text/event-stream")],
                            "data: {\"delta\":\"lorem ipsum\"}\n\ndata: [DONE]\n\n",
                        )
                    }
                }),
            );
        let (owner, address) = start(router, limits()).await?;
        let h1 = reqwest::Client::builder().http1_only().no_proxy().build()?;
        let response = h1
            .post(format!("http://{address}/echo"))
            .body("same API")
            .send()
            .await?;
        ensure!(
            response.version() == http::Version::HTTP_11 && response.text().await? == "same API"
        );
        let mut client = Client::connect(address).await?;
        let (response, mut upload) = client.request("/echo", false).await?;
        upload.send_data(Bytes::from_static(b"same API"), true)?;
        let response = response.await?;
        ensure!(response.version() == http::Version::HTTP_2);
        ensure!(collect(response.into_body()).await? == "same API");
        let mut responses = Vec::new();
        for _ in 0..128 {
            responses.push(client.request("/sse", true).await?.0);
        }
        for response in responses {
            let response = response.await?;
            ensure!(response.headers()[header::CONTENT_TYPE] == "text/event-stream");
            ensure!(
                collect(response.into_body()).await?
                    == "data: {\"delta\":\"lorem ipsum\"}\n\ndata: [DONE]\n\n"
            );
        }
        ensure!(
            owner.report().connections.started == 2,
            "HTTP/2 opened extra connections"
        );
        client.close().await?;
        stop(owner).await
    })
    .await?
}

#[tokio::test]
async fn slow_ingress_times_out_only_its_stream_and_not_inference() -> Result<()> {
    timeout(Duration::from_secs(10), async {
        let router = Router::new()
            .route("/echo", post(|body: Bytes| async move { body }))
            .route(
                "/inference",
                post(|| async {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    "completed inference"
                }),
            );
        let (owner, address) = start(router, limits()).await?;
        let mut client = Client::connect(address).await?;
        let (slow, mut upload) = client.request("/echo", false).await?;
        upload.send_data(Bytes::from_static(b"incomplete"), false)?;
        let (inference, _) = client.request("/inference", true).await?;
        let response = slow.await?;
        ensure!(response.status() == StatusCode::REQUEST_TIMEOUT);
        ensure!(collect(inference.await?.into_body()).await? == "completed inference");
        ensure!(client.request("/echo", true).await?.0.await?.status() == StatusCode::OK);
        client.close().await?;
        stop(owner).await
    })
    .await?
}

#[tokio::test]
async fn unread_response_resets_only_its_stream_and_releases_capacity() -> Result<()> {
    timeout(Duration::from_secs(10), async {
        let router = Router::new()
            .route("/large", post(|| async { vec![b'x'; 512 * 1024] }))
            .route("/ok", post(|| async { "healthy" }));
        let (owner, address) = start(router, limits()).await?;
        let mut client = Client::connect(address).await?;
        let response = client
            .request("/large", true)
            .await?
            .0
            .await
            .context("large response head")?;
        let mut unread = response.into_body();
        tokio::time::sleep(Duration::from_millis(450)).await;
        let failed = loop {
            match unread.data().await {
                Some(Ok(data)) => {
                    unread.flow_control().release_capacity(data.len())?;
                }
                Some(Err(error)) => break error.is_reset(),
                None => break false,
            }
        };
        ensure!(failed, "slow reader did not receive a stream reset");
        drop(unread);
        ensure!(
            collect(client.request("/ok", true).await?.0.await?.into_body()).await? == "healthy"
        );
        client.close().await?;
        stop(owner).await
    })
    .await?
}

struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn global_stream_limit_and_force_join_include_pending_handlers() -> Result<()> {
    timeout(Duration::from_secs(10), async {
        let active = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Notify::new());
        let observed = active.clone();
        let signal = entered.clone();
        let router = Router::new().route(
            "/",
            post(move || {
                let active = active.clone();
                let entered = entered.clone();
                async move {
                    active.fetch_add(1, Ordering::SeqCst);
                    let _active = Active(active);
                    entered.notify_one();
                    std::future::pending::<()>().await;
                    StatusCode::OK
                }
            }),
        );
        let (owner, address) = start(
            router,
            Limits {
                requests: 1,
                ..limits()
            },
        )
        .await?;
        let mut first = Client::connect(address).await?;
        let (_pending, _) = first.request("/", true).await?;
        signal.notified().await;
        let mut second = Client::connect(address).await?;
        ensure!(
            second.request("/", true).await?.0.await?.status() == StatusCode::SERVICE_UNAVAILABLE
        );
        ensure!(observed.load(Ordering::SeqCst) == 1);
        owner.request_force();
        let report = owner.join_forced().await?;
        ensure!(
            report.is_quiescent() && observed.load(Ordering::SeqCst) == 0,
            "force join left a detached HTTP/2 service"
        );
        first.driver.abort();
        second.driver.abort();
        let _ = first.driver.await;
        let _ = second.driver.await;
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn reset_cancels_handler_and_graceful_shutdown_delivers_active_response() -> Result<()> {
    timeout(Duration::from_secs(10), async {
        let active = Arc::new(AtomicUsize::new(0));
        let observed = active.clone();
        let entered = Arc::new(Notify::new());
        let signal = entered.clone();
        let release = Arc::new(Notify::new());
        let resume = release.clone();
        let router = Router::new().route(
            "/",
            post(move || {
                let entered = entered.clone();
                let active = active.clone();
                let release = release.clone();
                async move {
                    active.fetch_add(1, Ordering::SeqCst);
                    let _active = Active(active);
                    entered.notify_one();
                    release.notified().await;
                    "drained"
                }
            }),
        );
        let (owner, address) = start(router, limits()).await?;
        let mut client = Client::connect(address).await?;
        let (_cancelled, mut send) = client.request("/", true).await?;
        signal.notified().await;
        send.send_reset(Reason::CANCEL);
        while observed.load(Ordering::SeqCst) > 0 {
            tokio::task::yield_now().await;
        }
        let (response, _) = client.request("/", true).await?;
        signal.notified().await;
        let deadline = Instant::now() + Duration::from_secs(5);
        owner.stop(deadline)?;
        resume.notify_one();
        ensure!(collect(response.await?.into_body()).await? == "drained");
        ensure!(owner.drain_until(deadline).await?.is_clean());
        client.driver.abort();
        let _ = client.driver.await;
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn incomplete_preface_expires_and_oversized_request_is_stream_local() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    timeout(Duration::from_secs(10), async {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let router = Router::new().route(
            "/",
            post(move |_: Bytes| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { StatusCode::OK }
            }),
        );
        let (owner, address) = start(
            router,
            Limits {
                header_timeout: Duration::from_millis(100),
                ..limits()
            },
        )
        .await?;
        let mut partial = TcpStream::connect(address).await?;
        partial.write_all(b"PRI * HTTP/2.0\r\n").await?;
        ensure!(partial.read(&mut [0; 1]).await? == 0);
        let mut client = Client::connect(address).await?;
        poll_fn(|cx| client.sender.poll_ready(cx)).await?;
        let (response, _send) = client.sender.send_request(
            Request::builder()
                .method("POST")
                .uri("http://fixture/")
                .header(header::CONTENT_LENGTH, "999999999")
                .body(())?,
            false,
        )?;
        ensure!(response.await?.status() == StatusCode::PAYLOAD_TOO_LARGE);
        ensure!(observed.load(Ordering::SeqCst) == 0);
        ensure!(client.request("/", true).await?.0.await?.status() == StatusCode::OK);
        client.close().await?;
        stop(owner).await
    })
    .await?
}

#[tokio::test]
async fn five_hundred_in_flight_streams_complete_on_four_connections() -> Result<()> {
    timeout(Duration::from_secs(20), async {
        let entered = Arc::new(Barrier::new(500));
        let active = Arc::new(AtomicUsize::new(0));
        let observed = active.clone();
        let router = Router::new().route(
            "/sse",
            post(move || {
                let entered = entered.clone();
                let active = active.clone();
                async move {
                    active.fetch_add(1, Ordering::SeqCst);
                    let guard = Active(active);
                    entered.wait().await;
                    let stream = async_stream::stream! {
                        let _guard = guard;
                        for _ in 0..8 {
                            yield Ok::<_, Infallible>(Bytes::from_static(b"data: lorem ipsum\n\n"));
                            tokio::task::yield_now().await;
                        }
                        yield Ok::<_, Infallible>(Bytes::from_static(b"data: [DONE]\n\n"));
                    };
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(stream),
                    )
                }
            }),
        );
        let (owner, address) = start(
            router,
            Limits {
                connections: 4,
                requests: 512,
                write_timeout: Duration::from_secs(5),
                ..limits()
            },
        )
        .await?;
        let mut clients = Vec::new();
        let mut responses = Vec::new();
        for _ in 0..4 {
            let mut client = Client::connect(address).await?;
            for _ in 0..125 {
                responses.push(client.request("/sse", true).await?.0);
            }
            clients.push(client);
        }
        let expected = format!("{}data: [DONE]\n\n", "data: lorem ipsum\n\n".repeat(8));
        let results = futures::future::join_all(responses.into_iter().map(|response| async {
            let response = response.await?;
            ensure!(response.status() == StatusCode::OK);
            ensure!(collect(response.into_body()).await? == expected);
            Ok::<_, anyhow::Error>(())
        }))
        .await;
        for result in results {
            result?;
        }
        ensure!(observed.load(Ordering::SeqCst) == 0);
        ensure!(owner.report().connections.started == 4);
        for client in clients {
            client.close().await?;
        }
        stop(owner).await
    })
    .await?
}
