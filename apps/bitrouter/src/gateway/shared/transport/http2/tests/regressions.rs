use super::*;

use std::io;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::task::{Context, Poll};

use http::{HeaderMap, HeaderValue};
use http_body::{Body as HttpBody, Frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadBuf};

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let length = (payload.len() as u32).to_be_bytes();
    let mut bytes = length[1..].to_vec();
    bytes.extend_from_slice(&[kind, flags]);
    bytes.extend_from_slice(&stream.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn request_frame(stream: u32) -> Vec<u8> {
    // Indexed :method POST, :scheme http, :path /, literal :authority fixture.
    frame(1, 5, stream, b"\x83\x86\x84\x01\x07fixture")
}

async fn yields() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

fn duplex_server(router: Router, limits: Limits) -> (JoinHandle<()>, DuplexStream) {
    let (socket, peer) = tokio::io::duplex(4096);
    let slots = Arc::new(Semaphore::new(limits.requests));
    let task = tokio::spawn(super::super::accept(
        socket,
        router,
        limits,
        CancellationToken::new(),
        slots,
    ));
    (task, peer)
}

async fn raw_handshake(peer: &mut DuplexStream) -> Result<()> {
    let mut bytes = PREFACE.to_vec();
    bytes.extend(frame(4, 0, 0, &[]));
    peer.write_all(&bytes).await?;
    Ok(())
}

async fn read_frame(peer: &mut DuplexStream) -> Result<(u8, u8, u32, Vec<u8>)> {
    let mut header = [0; 9];
    peer.read_exact(&mut header).await?;
    let length =
        usize::from(header[0]) << 16 | usize::from(header[1]) << 8 | usize::from(header[2]);
    let id = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) & 0x7fff_ffff;
    let mut payload = vec![0; length];
    peer.read_exact(&mut payload).await?;
    Ok((header[3], header[4], id, payload))
}

async fn raw_response(peer: &mut DuplexStream, stream: u32) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let (kind, flags, id, payload) = read_frame(peer).await?;
        ensure!(
            kind != 7 && !(kind == 3 && id == stream),
            "unexpected reset/GOAWAY"
        );
        if kind == 4 && flags == 0 {
            peer.write_all(&frame(4, 1, 0, &[])).await?;
        }
        if id == stream {
            if kind == 0 {
                body.extend(payload);
            }
            if matches!(kind, 0 | 1) && flags & 1 != 0 {
                return Ok(body);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn complete_preface_and_incomplete_settings_expire_without_entering_router() -> Result<()> {
    for suffix in [
        Vec::new(),
        frame(4, 0, 0, &[0, 1, 0, 0, 0, 0])[..12].to_vec(),
    ] {
        let called = Arc::new(AtomicBool::new(false));
        let observed = called.clone();
        let router = Router::new().route(
            "/",
            post(move || {
                called.store(true, Ordering::SeqCst);
                async { "unexpected" }
            }),
        );
        let (mut task, mut peer) = duplex_server(router, limits());
        let mut bytes = PREFACE.to_vec();
        bytes.extend(suffix);
        peer.write_all(&bytes).await?;
        yields().await;
        tokio::time::advance(Duration::from_millis(1500)).await;
        yields().await;
        ensure!(
            !task.is_finished(),
            "connection closed before its header deadline"
        );
        tokio::time::advance(Duration::from_millis(501)).await;
        timeout(Duration::from_millis(1), &mut task).await??;
        ensure!(!observed.load(Ordering::SeqCst));
        let mut output = Vec::new();
        peer.read_to_end(&mut output).await?;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn fragmented_headers_after_a_valid_request_expire_but_idle_connections_survive() -> Result<()>
{
    for partial in [
        frame(4, 0, 0, &[0, 1, 0, 0, 0, 0])[..12].to_vec(),
        request_frame(3)[..5].to_vec(),
        request_frame(3)[..11].to_vec(),
        frame(1, 1, 3, b"\x83\x86\x84"),
    ] {
        let router = Router::new().route("/", post(|| async { "healthy" }));
        let (mut task, mut peer) = duplex_server(router, limits());
        raw_handshake(&mut peer).await?;
        peer.write_all(&request_frame(1)).await?;
        ensure!(raw_response(&mut peer, 1).await? == b"healthy");
        tokio::time::advance(Duration::from_secs(20)).await;
        yields().await;
        ensure!(
            !task.is_finished(),
            "idle connection inherited a header timer"
        );
        peer.write_all(&partial).await?;
        yields().await;
        tokio::time::advance(Duration::from_millis(1500)).await;
        yields().await;
        ensure!(!task.is_finished(), "fragmented header closed too early");
        tokio::time::advance(Duration::from_millis(501)).await;
        timeout(Duration::from_millis(1), &mut task).await??;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn delayed_first_http1_byte_does_not_restart_the_absolute_header_deadline() -> Result<()> {
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    let router = Router::new().route(
        "/",
        post(move || {
            called.store(true, Ordering::SeqCst);
            async { "too late" }
        }),
    );
    let (mut task, mut peer) = duplex_server(router, limits());
    yields().await;
    tokio::time::advance(Duration::from_millis(1500)).await;
    peer.write_all(b"P").await?;
    yields().await;
    tokio::time::advance(Duration::from_millis(400)).await;
    yields().await;
    ensure!(!task.is_finished(), "HTTP/1 header deadline fired early");
    tokio::time::advance(Duration::from_millis(101)).await;
    timeout(Duration::from_millis(1), &mut task).await??;
    ensure!(!observed.load(Ordering::SeqCst));
    Ok(())
}

struct ProbeBody {
    first: Option<Frame<Bytes>>,
    pending: bool,
    polled: Arc<Notify>,
    dropped: Arc<AtomicBool>,
}

impl HttpBody for ProbeBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.polled.notify_one();
        if let Some(frame) = self.first.take() {
            Poll::Ready(Some(Ok(frame)))
        } else if self.pending {
            Poll::Pending
        } else {
            Poll::Ready(None)
        }
    }
}

impl Drop for ProbeBody {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn queued_sensitive_heads_and_trailers_exhaust_budget_despite_trickling_output() -> Result<()>
{
    for trailers in [false, true] {
        for trickle in [false, true] {
            let mut headers = HeaderMap::new();
            let mut value = HeaderValue::from_bytes(&vec![b'x'; 60 * 1024])?;
            value.set_sensitive(true);
            headers.insert("x-private", value);
            let entered = Arc::new(AtomicUsize::new(0));
            let produced = Arc::new(AtomicUsize::new(0));
            let release = Arc::new(Semaphore::new(0));
            let calls = entered.clone();
            let completed = produced.clone();
            let gate = release.clone();
            let router = Router::new().route(
                "/",
                post(move || {
                    let headers = headers.clone();
                    let gate = gate.clone();
                    let completed = completed.clone();
                    calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        let Ok(permit) = gate.acquire().await else {
                            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                        };
                        permit.forget();
                        completed.fetch_add(1, Ordering::SeqCst);
                        if trailers {
                            Body::new(ProbeBody {
                                first: Some(Frame::trailers(headers)),
                                pending: false,
                                polled: Arc::default(),
                                dropped: Arc::default(),
                            })
                            .into_response()
                        } else {
                            (headers, Body::empty()).into_response()
                        }
                    }
                }),
            );
            let (mut task, mut peer) = duplex_server(
                router,
                Limits {
                    write_timeout: if trickle {
                        Duration::from_millis(100)
                    } else {
                        Duration::from_secs(60)
                    },
                    ..limits()
                },
            );
            raw_handshake(&mut peer).await?;
            for id in (1..49).step_by(2) {
                peer.write_all(&request_frame(id)).await?;
            }
            yields().await;
            ensure!(
                entered.load(Ordering::SeqCst) == 24,
                "not all handlers were admitted"
            );
            let began = Instant::now();
            for _ in 0..24 {
                release.add_permits(1);
                yields().await;
                if task.is_finished() {
                    break;
                }
                if trickle {
                    // A successful one-byte write renews the write-stall timer,
                    // but cannot retire a 60 KiB decoded header reservation.
                    let _ = peer.read_u8().await?;
                    yields().await;
                    tokio::time::advance(Duration::from_millis(20)).await;
                }
            }
            timeout(Duration::from_millis(1), &mut task).await??;
            let count = produced.load(Ordering::SeqCst);
            ensure!(
                (16..24).contains(&count),
                "connection ended outside metadata pressure: {count}"
            );
            if trickle {
                ensure!(Instant::now() - began > Duration::from_millis(100));
            }
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn streaming_echo_times_out_unfinished_upload_after_response_headers_and_keeps_sibling()
-> Result<()> {
    timeout(Duration::from_secs(10), async {
        let router = Router::new()
            .route("/echo", post(|body: Body| async move { body }))
            .route("/ok", post(|| async { "healthy" }));
        let (task, peer) = duplex_server(router, limits());
        let (sender, connection) = h2::client::handshake(peer).await?;
        let mut client = Client {
            sender,
            driver: tokio::spawn(connection),
        };
        let (response, mut upload) = client.request("/echo", false).await?;
        upload.send_data(Bytes::from_static(b"first chunk"), false)?;
        let response = response.await?;
        ensure!(response.status() == StatusCode::OK);
        let mut echo = response.into_body();
        let first = echo
            .data()
            .await
            .context("echo omitted its first chunk")??;
        ensure!(first == "first chunk");
        echo.flow_control().release_capacity(first.len())?;
        ensure!(
            collect(client.request("/ok", true).await?.0.await?.into_body()).await? == "healthy"
        );
        // Retain the upload handle and withhold END_STREAM. The response is
        // already 200, so expiry must reset this stream rather than emit 408.
        tokio::time::advance(Duration::from_millis(201)).await;
        let reset = timeout(Duration::from_secs(2), echo.data()).await?;
        ensure!(
            matches!(reset, Some(Err(error)) if error.is_reset()),
            "unfinished echoed upload was not reset"
        );
        ensure!(
            collect(client.request("/ok", true).await?.0.await?.into_body()).await? == "healthy"
        );
        drop(upload);
        client.close().await?;
        task.await?;
        Ok(())
    })
    .await?
}

#[tokio::test(start_paused = true)]
async fn cancelled_overload_on_second_connection_keeps_a_healthy_sibling() -> Result<()> {
    timeout(Duration::from_secs(10), async {
        let entered = Arc::new(Notify::new());
        let observed = entered.clone();
        let release = Arc::new(Notify::new());
        let resume = release.clone();
        let router = Router::new()
            .route(
                "/hold",
                post(move || {
                    let entered = entered.clone();
                    let release = release.clone();
                    async move {
                        entered.notify_one();
                        release.notified().await;
                        "released"
                    }
                }),
            )
            .route("/", post(|| async { "healthy" }));
        let shared_slots = Arc::new(Semaphore::new(2));
        let config = Limits {
            requests: 2,
            ..limits()
        };
        let (first_socket, first_peer) = tokio::io::duplex(4096);
        let first_task = tokio::spawn(super::super::accept(
            first_socket,
            router.clone(),
            config,
            CancellationToken::new(),
            shared_slots.clone(),
        ));
        let (sender, connection) = h2::client::handshake(first_peer).await?;
        let mut first = Client {
            sender,
            driver: tokio::spawn(connection),
        };
        let (held, _) = first.request("/hold", true).await?;
        observed.notified().await;
        let (also_held, _) = first.request("/hold", true).await?;
        observed.notified().await;
        ensure!(shared_slots.available_permits() == 0);
        let (second_socket, mut second) = tokio::io::duplex(4096);
        let second_task = tokio::spawn(super::super::accept(
            second_socket,
            router,
            config,
            CancellationToken::new(),
            shared_slots.clone(),
        ));
        // Queue HEADERS, its RST_STREAM, and a sibling atomically before h2 is
        // polled. The first accepted request is already reset when overload
        // handling attempts send_response, exercising the stream-local error.
        let mut packet = PREFACE.to_vec();
        packet.extend(frame(4, 0, 0, &[]));
        packet.extend(request_frame(1));
        packet.extend(frame(3, 0, 1, &[0, 0, 0, 8]));
        packet.extend(request_frame(3));
        second.write_all(&packet).await?;
        ensure!(raw_response(&mut second, 3).await?.is_empty());
        ensure!(!second_task.is_finished());
        resume.notify_waiters();
        ensure!(collect(held.await?.into_body()).await? == "released");
        ensure!(collect(also_held.await?.into_body()).await? == "released");
        ensure!(shared_slots.available_permits() == 2);
        second.write_all(&request_frame(5)).await?;
        ensure!(raw_response(&mut second, 5).await? == b"healthy");
        first.close().await?;
        drop(second);
        first_task.await?;
        second_task.await?;
        Ok(())
    })
    .await?
}

#[tokio::test(start_paused = true)]
async fn completed_ingress_allows_slow_inference_and_delayed_response_chunks() -> Result<()> {
    let router = Router::new().route(
        "/",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Body::from_stream(async_stream::stream! {
                yield Ok::<_, Infallible>(Bytes::from_static(b"first "));
                tokio::time::sleep(Duration::from_secs(10)).await;
                yield Ok::<_, Infallible>(Bytes::from_static(b"second"));
            })
        }),
    );
    let (task, peer) = duplex_server(router, limits());
    let (sender, connection) = h2::client::handshake(peer).await?;
    let mut client = Client {
        sender,
        driver: tokio::spawn(connection),
    };
    let began = Instant::now();
    let response = client.request("/", true).await?.0.await?;
    ensure!(collect(response.into_body()).await? == "first second");
    ensure!(Instant::now() - began >= Duration::from_secs(20));
    client.close().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn force_joins_pending_response_bodies_and_zero_window_sends() -> Result<()> {
    timeout(Duration::from_secs(10), async {
        for zero_window in [false, true] {
            let polled = Arc::new(Notify::new());
            let observed = polled.clone();
            let dropped = Arc::new(AtomicBool::new(false));
            let released = dropped.clone();
            let router = Router::new().route(
                "/",
                post(move || {
                    let body = ProbeBody {
                        first: zero_window
                            .then(|| Frame::data(Bytes::from(vec![b'x'; 128 * 1024]))),
                        pending: true,
                        polled: polled.clone(),
                        dropped: dropped.clone(),
                    };
                    async move { Body::new(body) }
                }),
            );
            let (owner, address) = start(
                router,
                Limits {
                    write_timeout: Duration::from_secs(60),
                    ..limits()
                },
            )
            .await?;
            let mut builder = h2::client::Builder::new();
            if zero_window {
                builder.initial_window_size(0);
            }
            let (sender, connection) = builder
                .handshake(TcpStream::connect(address).await?)
                .await?;
            let mut client = Client {
                sender,
                driver: tokio::spawn(connection),
            };
            let response = client.request("/", true).await?.0.await?;
            observed.notified().await;
            ensure!(!released.load(Ordering::SeqCst));
            owner.request_force();
            let report = timeout(Duration::from_secs(1), owner.join_forced()).await??;
            ensure!(report.is_quiescent() && report.connections.cancelled == 1);
            ensure!(
                released.load(Ordering::SeqCst),
                "force left a response body retained"
            );
            drop(response);
            client.driver.abort();
            let _ = client.driver.await;
        }
        Ok(())
    })
    .await?
}

struct FlushGate {
    inner: DuplexStream,
    blocked: Arc<AtomicBool>,
    polled: Arc<Notify>,
    dropped: Arc<AtomicBool>,
}

impl AsyncRead for FlushGate {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for FlushGate {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.blocked.load(Ordering::SeqCst) {
            self.polled.notify_one();
            Poll::Pending
        } else {
            Pin::new(&mut self.inner).poll_flush(cx)
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl Drop for FlushGate {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn force_cancels_connection_retained_by_pending_final_flush() -> Result<()> {
    let blocked = Arc::new(AtomicBool::new(false));
    let block = blocked.clone();
    let flushed = Arc::new(Notify::new());
    let io_dropped = Arc::new(AtomicBool::new(false));
    let body_dropped = Arc::new(AtomicBool::new(false));
    let released = body_dropped.clone();
    let router = Router::new().route(
        "/",
        post(move || {
            block.store(true, Ordering::SeqCst);
            let body = ProbeBody {
                first: Some(Frame::data(Bytes::from_static(b"final chunk"))),
                pending: false,
                polled: Arc::default(),
                dropped: body_dropped.clone(),
            };
            async move { Body::new(body) }
        }),
    );
    let (socket, peer) = tokio::io::duplex(4096);
    let task = tokio::spawn(super::super::accept(
        FlushGate {
            inner: socket,
            blocked,
            polled: flushed.clone(),
            dropped: io_dropped.clone(),
        },
        router,
        Limits {
            write_timeout: Duration::from_secs(60),
            ..limits()
        },
        CancellationToken::new(),
        Arc::new(Semaphore::new(1)),
    ));
    let (sender, connection) = h2::client::handshake(peer).await?;
    let mut client = Client {
        sender,
        driver: tokio::spawn(connection),
    };
    let (_response, _) = client.request("/", true).await?;
    flushed.notified().await;
    ensure!(
        released.load(Ordering::SeqCst),
        "body has not reached final flush"
    );
    ensure!(!task.is_finished() && !io_dropped.load(Ordering::SeqCst));
    // Transport::request_force aborts this retained connection root. Injected
    // duplex I/O makes its final flush controllable without a TCP-buffer guess.
    task.abort();
    let result = task.await;
    ensure!(matches!(result, Err(error) if error.is_cancelled()));
    ensure!(
        io_dropped.load(Ordering::SeqCst),
        "force left the socket retained"
    );
    client.driver.abort();
    let _ = client.driver.await;
    Ok(())
}
