use super::*;

use anyhow::ensure;
use http::HeaderValue;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::response::IntoResponse;
use axum::routing::post;
use futures::FutureExt;
use http_body::Frame as BodyFrame;

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const STALL: Duration = Duration::from_secs(2);

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let length = (payload.len() as u32).to_be_bytes();
    let mut bytes = Vec::from(&length[1..]);
    bytes.extend_from_slice(&[kind, flags]);
    bytes.extend_from_slice(&stream.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn large_headers() -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    let mut value = HeaderValue::from_bytes(&vec![b'x'; 60 * 1024])?;
    value.set_sensitive(true);
    headers.insert("x-private", value);
    Ok(headers)
}

// The 11-byte socket forces both frame headers and payloads to cross writes.
// Admission is checked before every actual write, including the last byte.
async fn write_block(
    wire: &mut Wire<DuplexStream>,
    peer: &mut DuplexStream,
    bytes: &[u8],
    vectored: bool,
    large: &HeaderMap,
) -> anyhow::Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        ensure!(
            !wire.headers.reserve(99, large),
            "header budget was released before END_HEADERS reached the peer"
        );
        let pending = &bytes[offset..];
        let written = if vectored {
            let split = pending.len().min(5);
            wire.write_vectored(&[
                IoSlice::new(&pending[..split]),
                IoSlice::new(&pending[split..]),
            ])
            .await?
        } else {
            wire.write(pending).await?
        };
        ensure!(written > 0);
        let mut observed = vec![0; written];
        peer.read_exact(&mut observed).await?;
        ensure!(observed == pending[..written]);
        offset += written;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn fragmented_scalar_and_vectored_writes_release_heads_and_trailers_only_at_end_headers()
-> anyhow::Result<()> {
    for vectored in [false, true] {
        let headers = Arc::new(Headers::default());
        let large = large_headers()?;
        // One stream has both its response head and trailers queued.
        ensure!(headers.reserve(1, &large));
        ensure!(headers.reserve(1, &large));
        for id in (3..31).step_by(2) {
            ensure!(headers.reserve(id, &large));
        }
        let (socket, mut peer) = tokio::io::duplex(11);
        let mut wire = Wire::new(socket, headers.clone(), Instant::now() + STALL, STALL);
        let mut head = frame(1, 0, 1, &[0x88; 19]);
        head.extend(frame(9, 4, 1, &[0; 17]));
        write_block(&mut wire, &mut peer, &head, vectored, &large).await?;
        ensure!(
            headers.reserve(99, &large),
            "completed head kept its reservation"
        );
        ensure!(
            !headers.reserve(101, &large),
            "head completion also released trailers"
        );

        let mut trailers = frame(1, 1, 1, &[0; 13]);
        // END_STREAM belongs to HEADERS; END_HEADERS arrives on CONTINUATION.
        trailers.extend(frame(9, 4, 1, &[0; 23]));
        write_block(&mut wire, &mut peer, &trailers, vectored, &large).await?;
        ensure!(
            headers.reserve(101, &large),
            "completed trailers kept their reservation"
        );
        ensure!(!headers.reserve(103, &large));
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cancelled_unsent_trailers_preserve_queued_head_until_reset_is_written()
-> anyhow::Result<()> {
    let headers = Arc::new(Headers::default());
    let large = large_headers()?;
    for id in (1..33).step_by(2) {
        ensure!(headers.reserve(id, &large));
    }
    ensure!(headers.reserve(1, &HeaderMap::new()));
    headers.unsent(1);
    ensure!(
        !headers.reserve(99, &large),
        "unsent trailers erased an older head"
    );
    let (socket, mut peer) = tokio::io::duplex(11);
    let mut wire = Wire::new(socket, headers.clone(), Instant::now() + STALL, STALL);
    write_block(
        &mut wire,
        &mut peer,
        &frame(3, 0, 1, &[0, 0, 0, 8]),
        true,
        &large,
    )
    .await?;
    ensure!(headers.reserve(99, &large));
    ensure!(
        !headers.reserve(101, &large),
        "reset released another stream's head"
    );
    Ok(())
}

async fn feed(
    wire: &mut Wire<DuplexStream>,
    peer: &mut DuplexStream,
    bytes: &[u8],
) -> anyhow::Result<()> {
    peer.write_all(bytes).await?;
    let mut observed = vec![0; bytes.len()];
    wire.read_exact(&mut observed).await?;
    ensure!(observed == bytes);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn preface_and_partial_settings_keep_the_original_deadline() -> anyhow::Result<()> {
    for suffix in [
        Vec::new(),
        frame(4, 0, 0, &[0, 1, 0, 0, 0, 0])[..12].to_vec(),
    ] {
        let (socket, mut peer) = tokio::io::duplex(128);
        let mut wire = Wire::new(socket, Arc::default(), Instant::now() + STALL, STALL);
        tokio::time::advance(Duration::from_millis(1500)).await;
        let mut initial = PREFACE.to_vec();
        initial.extend(suffix);
        feed(&mut wire, &mut peer, &initial).await?;
        tokio::time::advance(Duration::from_millis(501)).await;
        // Keep the peer open: EOF cannot masquerade as the timeout.
        let result = tokio::time::timeout(Duration::from_millis(1), wire.read_u8()).await?;
        ensure!(matches!(result, Err(error) if error.kind() == ErrorKind::TimedOut));
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn idle_reads_have_no_timer_but_trickled_continuations_do_not_renew_it() -> anyhow::Result<()>
{
    let (socket, mut peer) = tokio::io::duplex(256);
    let mut wire = Wire::new(socket, Arc::default(), Instant::now() + STALL, STALL);
    let mut initial = PREFACE.to_vec();
    initial.extend(frame(4, 0, 0, &[]));
    initial.extend(frame(1, 5, 1, &[0x83, 0x86, 0x84]));
    feed(&mut wire, &mut peer, &initial).await?;
    tokio::time::advance(Duration::from_secs(20)).await;
    ensure!(
        tokio::time::timeout(Duration::from_millis(1), wire.read_u8())
            .await
            .is_err()
    );
    feed(&mut wire, &mut peer, &frame(6, 0, 0, &[0; 8])).await?;
    feed(&mut wire, &mut peer, &frame(1, 1, 3, &[0x83])).await?;
    tokio::time::advance(Duration::from_millis(1500)).await;
    feed(&mut wire, &mut peer, &frame(9, 0, 3, &[0x86])).await?;
    tokio::time::advance(Duration::from_millis(501)).await;
    let result = tokio::time::timeout(Duration::from_millis(1), wire.read_u8()).await?;
    ensure!(matches!(result, Err(error) if error.kind() == ErrorKind::TimedOut));
    Ok(())
}

// These connections are explicitly polled by the tests. The flush gate needs
// no autonomous wakeup: each transition is followed by another connection poll.
struct FlushGate {
    inner: DuplexStream,
    mode: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
}

impl AsyncRead for FlushGate {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for FlushGate {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        match self.mode.load(Ordering::SeqCst) {
            1 => Poll::Pending,
            2 => Poll::Ready(Err(Error::other("injected flush failure"))),
            _ => Pin::new(&mut self.inner).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

struct Trailers(Option<HeaderMap>);

impl http_body::Body for Trailers {
    type Data = bytes::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<BodyFrame<Self::Data>, Self::Error>>> {
        Poll::Ready(
            self.0
                .take()
                .map(|headers| Ok(BodyFrame::trailers(headers))),
        )
    }
}

fn request(stream: u32) -> Vec<u8> {
    frame(1, 5, stream, b"\x83\x86\x84\x01\x07fixture")
}

fn stream_limits() -> super::super::super::Limits {
    super::super::super::Limits {
        connections: 1,
        requests: 128,
        request_bytes: 1024,
        header_timeout: STALL,
        request_timeout: STALL,
        write_timeout: STALL,
        drain_timeout: STALL,
    }
}

/// A real h2 peer reset must reclaim queued metadata even if h2 flushed before
/// the retained response future observed the reset. Overload uses the same path.
#[tokio::test(start_paused = true)]
async fn remote_reset_reclaims_queued_heads_for_admitted_and_overload_responses()
-> anyhow::Result<()> {
    for admitted in [true, false] {
        let headers = Arc::new(Headers::default());
        let (socket, mut peer) = tokio::io::duplex(4096);
        peer.write_all(PREFACE).await?;
        peer.write_all(&frame(4, 0, 0, &[])).await?;
        let mut connection = h2::server::handshake(Wire::new(
            socket,
            headers.clone(),
            Instant::now() + STALL,
            STALL,
        ))
        .await?;
        let called = Arc::new(AtomicUsize::new(0));
        let entered = called.clone();
        let router = axum::Router::new().route(
            "/",
            post(move || {
                entered.fetch_add(1, Ordering::SeqCst);
                async { http::StatusCode::OK }
            }),
        );
        for id in (1..65).step_by(2) {
            // Each iteration deliberately polls the driver only once per
            // transition; do not mistake cooperative yielding for socket state.
            tokio::task::yield_now().await;
            peer.write_all(&request(id)).await?;
            let (request, reply) =
                connection.accept().await.transpose()?.ok_or_else(|| {
                    anyhow::anyhow!("connection closed during cancellation sequence")
                })?;
            let response = super::super::stream(
                request,
                reply,
                router.clone(),
                stream_limits(),
                headers.clone(),
                admitted,
            );
            tokio::pin!(response);
            ensure!(futures::poll!(&mut response).is_pending());
            let charged = headers.lock().bytes;
            ensure!(charged >= 512, "response was not charged");

            // No connection poll has encoded the response yet. Process the
            // reset first; h2 drops the queue without writing an outbound RST.
            peer.write_all(&frame(3, 0, id, &[0, 0, 0, 8])).await?;
            ensure!(connection.accept().now_or_never().is_none());
            ensure!(
                headers.lock().bytes == charged,
                "raw reset released credits"
            );
            ensure!(futures::poll!(&mut response).is_pending());
            ensure!(
                headers.lock().cancelled.contains(&id),
                "reset was not observed for stream {id}, admitted={admitted}"
            );
            // An idle flush after late reset observation must still reclaim.
            ensure!(connection.accept().now_or_never().is_none());
            ensure!(headers.lock().bytes == 0, "cancelled queue kept its charge");
            ensure!(headers.lock().streams.is_empty());
            ensure!(headers.lock().cancelled.is_empty());
            response.await?;
        }
        ensure!(called.load(Ordering::SeqCst) == if admitted { 32 } else { 0 });
        ensure!(!headers.exhausted.is_cancelled());

        // A later sibling must still complete after all the cancellations.
        peer.write_all(&request(65)).await?;
        let (request, reply) = connection
            .accept()
            .await
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("healthy sibling was not accepted"))?;
        let response = super::super::stream(
            request,
            reply,
            router,
            stream_limits(),
            headers.clone(),
            true,
        );
        tokio::pin!(response);
        ensure!(futures::poll!(&mut response).is_pending());
        ensure!(connection.accept().now_or_never().is_none());
        response.await?;
        ensure!(headers.lock().bytes == 0);
    }
    Ok(())
}

/// Resetting an encoded head plus queued trailers must keep both charged until
/// END_HEADERS and then a successful flush, respectively.
#[tokio::test(start_paused = true)]
async fn remote_reset_keeps_partial_encoded_headers_charged_until_encoder_flush()
-> anyhow::Result<()> {
    let headers = Arc::new(Headers::default());
    let mode = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let (socket, mut peer) = tokio::io::duplex(64);
    peer.write_all(PREFACE).await?;
    peer.write_all(&frame(4, 0, 0, &[])).await?;
    let mut connection = h2::server::handshake(Wire::new(
        FlushGate {
            inner: socket,
            mode: mode.clone(),
            polls: polls.clone(),
        },
        headers.clone(),
        Instant::now() + STALL,
        STALL,
    ))
    .await?;
    let large = large_headers()?;
    let router = axum::Router::new().route(
        "/",
        post(move || {
            let head = large.clone();
            let trailers = large.clone();
            async move { (head, Body::new(Trailers(Some(trailers)))).into_response() }
        }),
    );
    peer.write_all(&request(1)).await?;
    let (request, reply) = connection
        .accept()
        .await
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("request not accepted"))?;
    let response = super::super::stream(
        request,
        reply,
        router,
        stream_limits(),
        headers.clone(),
        true,
    );
    tokio::pin!(response);
    ensure!(futures::poll!(&mut response).is_pending());
    let charged = headers.lock().bytes;
    ensure!(
        headers
            .lock()
            .streams
            .get(&1)
            .is_some_and(|queue| queue.len() == 2)
    );

    // Only 64 bytes fit: h2 owns the remaining HPACK bytes and CONTINUATIONs.
    ensure!(connection.accept().now_or_never().is_none());
    peer.write_all(&frame(3, 0, 1, &[0, 0, 0, 8])).await?;
    ensure!(connection.accept().now_or_never().is_none());
    ensure!(futures::poll!(&mut response).is_pending());
    ensure!(headers.lock().cancelled.contains(&1));
    ensure!(
        headers.lock().bytes == charged,
        "partial encoding was uncharged"
    );

    mode.store(1, Ordering::SeqCst);
    let before = polls.load(Ordering::SeqCst);
    let mut observed = Vec::new();
    for _ in 0..4096 {
        let mut bytes = [0; 64];
        if let Some(read) = peer.read(&mut bytes).now_or_never() {
            let count = read?;
            ensure!(count != 0);
            observed.extend_from_slice(&bytes[..count]);
        }
        ensure!(connection.accept().now_or_never().is_none());
        if polls.load(Ordering::SeqCst) > before {
            break;
        }
        ensure!(headers.lock().bytes > 0);
        tokio::task::yield_now().await;
    }
    ensure!(
        polls.load(Ordering::SeqCst) > before,
        "encoder never reached flush"
    );
    let remaining = headers.lock().bytes;
    ensure!(
        remaining > 0 && remaining < charged,
        "END_HEADERS must release the head while discarded trailers await flush"
    );
    ensure!(futures::poll!(&mut response).is_pending());
    ensure!(
        observed.len() > 16_384,
        "test did not exercise CONTINUATION output"
    );
    mode.store(0, Ordering::SeqCst);
    ensure!(connection.accept().now_or_never().is_none());
    ensure!(headers.lock().bytes == 0);
    response.await?;
    Ok(())
}

/// Failed/Pending flushes do not retire cancelled reservations, and a successful
/// flush wakes every completed response waiting for its own ledger entry.
#[tokio::test(start_paused = true)]
async fn cancelled_metadata_waiters_require_successful_flush_and_all_wake() -> anyhow::Result<()> {
    let headers = Arc::new(Headers::default());
    ensure!(headers.reserve(1, &HeaderMap::new()));
    ensure!(headers.reserve(3, &HeaderMap::new()));
    let one = headers.clone();
    let first = tokio::spawn(async move { one.released(1).await });
    let three = headers.clone();
    let second = tokio::spawn(async move { three.released(3).await });
    tokio::task::yield_now().await;
    headers.cancel(1);
    headers.cancel(3);
    let mode = Arc::new(AtomicUsize::new(1));
    let (socket, _peer) = tokio::io::duplex(64);
    let mut wire = Wire::new(
        FlushGate {
            inner: socket,
            mode: mode.clone(),
            polls: Arc::default(),
        },
        headers.clone(),
        Instant::now() + STALL,
        STALL,
    );
    ensure!(wire.flush().now_or_never().is_none());
    ensure!(headers.lock().bytes == 1024);
    mode.store(2, Ordering::SeqCst);
    ensure!(wire.flush().await.is_err());
    ensure!(headers.lock().bytes == 1024);
    ensure!(!first.is_finished() && !second.is_finished());
    mode.store(0, Ordering::SeqCst);
    wire.flush().await?;
    tokio::time::timeout(STALL, first).await??;
    tokio::time::timeout(STALL, second).await??;
    {
        let pending = headers.lock();
        ensure!(pending.bytes == 0 && pending.streams.is_empty());
    }
    // A subscriber arriving after notification also completes immediately.
    tokio::time::timeout(STALL, headers.released(1)).await?;
    Ok(())
}
