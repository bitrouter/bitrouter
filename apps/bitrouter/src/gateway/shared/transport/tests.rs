use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use anyhow::{Context as _, Result, ensure};
use axum::routing::{get, post};
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

use super::*;

mod bodies;
mod deadlines;
mod shutdown;

pub(super) fn limits() -> Limits {
    Limits {
        connections: 1,
        #[cfg(feature = "http2")]
        requests: 128,
        request_bytes: 1024,
        header_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(2),
        write_timeout: Duration::from_secs(2),
        drain_timeout: Duration::from_millis(100),
    }
}

struct Frames {
    frames: VecDeque<Frame<Bytes>>,
    dropped: Arc<AtomicBool>,
}

impl HttpBody for Frames {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        Poll::Ready(self.frames.pop_front().map(Ok))
    }
    fn is_end_stream(&self) -> bool {
        self.frames.is_empty()
    }
    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(
            self.frames
                .iter()
                .filter_map(Frame::data_ref)
                .map(|b| b.len() as u64)
                .sum(),
        )
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

fn frames(data: impl IntoIterator<Item = Frame<Bytes>>, dropped: Arc<AtomicBool>) -> Body {
    Body::new(Frames {
        frames: data.into_iter().collect(),
        dropped,
    })
}

async fn next<B: HttpBody<Data = Bytes> + Unpin>(
    body: &mut B,
) -> Option<Result<Frame<Bytes>, B::Error>> {
    std::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)).await
}

// The final frame is consumed by real Hyper before the peer drains its write
// buffer. A body-held request owner and a connection-held permit are distinct.
#[tokio::test(start_paused = true)]
async fn final_json_and_sse_frames_release_body_before_transport_tail() -> Result<()> {
    for event_stream in [false, true] {
        let dropped = Arc::new(AtomicBool::new(false));
        let observed = dropped.clone();
        let router = Router::new().route(
            "/",
            get(move || {
                let dropped = dropped.clone();
                async move {
                    let data = if event_stream {
                        format!("data: {}\n\n", "a".repeat(1024))
                    } else {
                        format!("{{\"value\":\"{}\"}}", "a".repeat(1024))
                    };
                    frames([Frame::data(Bytes::from(data))], dropped)
                }
            }),
        );
        let (server, mut peer) = tokio::io::duplex(64);
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().try_acquire_owned()?;
        let task = tokio::spawn(async move {
            let _permit = permit;
            connection(server, router, limits(), CancellationToken::new()).await;
        });
        peer.write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .await?;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        ensure!(
            observed.load(Ordering::SeqCst),
            "Hyper has not consumed final body frame"
        );
        ensure!(
            slots.available_permits() == 0 && !task.is_finished(),
            "body EOF released transport capacity"
        );
        // No reads or other socket events are needed to wake the stalled writer.
        tokio::time::advance(Duration::from_secs(3)).await;
        task.await?;
        ensure!(slots.available_permits() == 1);
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn pipelined_early_responses_cannot_accumulate_heads() -> Result<()> {
    for vectored in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let router = Router::new().route(
            "/",
            post(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                async {
                    (
                        [(
                            http::header::HeaderName::from_static("x-fixture"),
                            "x".repeat(6000),
                        )],
                        StatusCode::UNAUTHORIZED,
                    )
                }
            }),
        );
        let (server, peer) = tokio::io::duplex(64);
        let task = tokio::spawn(connection(
            deadlines::Vectored {
                inner: server,
                enabled: vectored,
            },
            router,
            limits(),
            CancellationToken::new(),
        ));
        // Pipelined input includes an unread body. Sending in a separate owned
        // task allows the request side to block after Hyper closes its read side.
        let (mut read, mut write) = tokio::io::split(peer);
        let writer = tokio::spawn(async move {
            write
                .write_all(
                    b"POST / HTTP/1.1\r\nHost: fixture\r\nContent-Length: 1\r\n\r\nx"
                        .repeat(100)
                        .as_slice(),
                )
                .await
        });
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        ensure!(calls.load(Ordering::SeqCst) == 1);
        for _ in 0..3 {
            let mut byte = [0u8; 1];
            read.read_exact(&mut byte).await?;
            tokio::time::advance(Duration::from_millis(500)).await;
            ensure!(
                calls.load(Ordering::SeqCst) == 1,
                "positive writes admitted a second head"
            );
        }
        tokio::time::advance(Duration::from_secs(3)).await;
        task.await?;
        writer.abort();
        let _ = writer.await;
        ensure!(calls.load(Ordering::SeqCst) == 1);
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn ingress_timeout_does_not_become_an_inference_timeout() -> Result<()> {
    let entered = Arc::new(Notify::new());
    let ready = entered.clone();
    let router = Router::new().route(
        "/",
        post(move |_: Bytes| {
            let entered = entered.clone();
            async move {
                entered.notify_one();
                std::future::pending::<()>().await;
                StatusCode::OK
            }
        }),
    );
    let (server, mut peer) = tokio::io::duplex(1024);
    let stop = CancellationToken::new();
    let task = tokio::spawn(connection(server, router, limits(), stop.clone()));
    peer.write_all(b"POST / HTTP/1.1\r\nHost: fixture\r\nContent-Length: 1\r\n\r\nx")
        .await?;
    ready.notified().await;
    tokio::time::advance(Duration::from_secs(60)).await;
    ensure!(!task.is_finished(), "completed ingress timed out inference");
    // Direct connection helper has no drain owner; abort and join its service.
    task.abort();
    ensure!(task.await.is_err());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn slow_headers_and_incomplete_bodies_close() -> Result<()> {
    for data in [
        b"POST / HTTP/1.1\r\nHost: fixture\r\n".as_slice(),
        b"POST / HTTP/1.1\r\nHost: fixture\r\nContent-Length: 2\r\n\r\nx".as_slice(),
    ] {
        let router = Router::new().route("/", post(|_: Bytes| async { StatusCode::OK }));
        let (server, mut peer) = tokio::io::duplex(1024);
        let task = tokio::spawn(connection(
            server,
            router,
            limits(),
            CancellationToken::new(),
        ));
        peer.write_all(data).await?;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(3)).await;
        task.await?;
        let mut output = Vec::new();
        peer.read_to_end(&mut output).await?;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn oversized_known_length_is_rejected_before_service() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let router = Router::new().route(
        "/",
        post(move |_: Bytes| {
            counted.fetch_add(1, Ordering::SeqCst);
            async { StatusCode::OK }
        }),
    );
    let (server, mut peer) = tokio::io::duplex(1024);
    let task = tokio::spawn(connection(
        server,
        router,
        limits(),
        CancellationToken::new(),
    ));
    peer.write_all(b"POST / HTTP/1.1\r\nHost: fixture\r\nContent-Length: 999999999999\r\n\r\n")
        .await?;
    let mut output = Vec::new();
    peer.read_to_end(&mut output).await?;
    task.await?;
    ensure!(output.starts_with(b"HTTP/1.1 413"));
    ensure!(calls.load(Ordering::SeqCst) == 0);
    Ok(())
}

#[tokio::test]
async fn overload_closes_without_queued_service_and_shutdown_joins() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let entered = Arc::new(Notify::new());
        let observed = entered.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let held = active.clone();
        struct Active(Arc<AtomicUsize>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let router = Router::new().route(
            "/",
            get(move || {
                let held = held.clone();
                let entered = entered.clone();
                async move {
                    held.fetch_add(1, Ordering::SeqCst);
                    let _active = Active(held);
                    entered.notify_one();
                    std::future::pending::<()>().await;
                    StatusCode::OK
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let stop = CancellationToken::new();
        let task = tokio::spawn(serve(
            listener,
            router,
            limits(),
            stop.clone().cancelled_owned(),
        ));
        let mut first = tokio::net::TcpStream::connect(address).await?;
        first
            .write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .await?;
        observed.notified().await;
        for _ in 0..20 {
            let mut excess = tokio::net::TcpStream::connect(address).await?;
            let mut bytes = [0u8; 1];
            ensure!(excess.read(&mut bytes).await? == 0);
            ensure!(active.load(Ordering::SeqCst) == 1);
        }
        stop.cancel();
        task.await??;
        ensure!(
            active.load(Ordering::SeqCst) == 0,
            "shutdown detached Router future"
        );
        Ok(())
    })
    .await
    .context("connection owner regression timed out")?
}
