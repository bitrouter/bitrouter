//! HTTP/1 response flush custody. One pending exchange per connection prevents
//! pipelined response heads from accumulating outside Hyper's DATA threshold.

use std::io::{IoSlice, Result};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Notify, OwnedSemaphorePermit};

#[derive(Default)]
struct State {
    permit: Option<OwnedSemaphorePermit>,
    response_done: bool,
}

#[derive(Default)]
pub(super) struct Exchange {
    state: Mutex<State>,
    flushed: Notify,
}

impl Exchange {
    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(error) => error.into_inner(),
        }
    }

    pub(super) async fn ready(&self) {
        // Only one Hyper service future exists per HTTP/1 connection. notify_one
        // retains the notification if the previous flush precedes this await.
        loop {
            if self.lock().permit.is_none() {
                return;
            }
            self.flushed.notified().await;
        }
    }

    pub(super) fn start(&self, permit: OwnedSemaphorePermit) {
        let mut state = self.lock();
        state.response_done = false;
        state.permit = Some(permit);
    }

    pub(super) fn body_done(&self) {
        self.lock().response_done = true;
    }

    fn flushed(&self) {
        let permit = {
            let mut state = self.lock();
            if !state.response_done {
                return;
            }
            state.permit.take()
        };
        if permit.is_some() {
            drop(permit);
            self.flushed.notify_one();
        }
    }
}

pub(super) struct Flushed<I> {
    inner: I,
    exchange: Arc<Exchange>,
}

impl<I> Flushed<I> {
    pub(super) fn new(inner: I, exchange: Arc<Exchange>) -> Self {
        Self { inner, exchange }
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for Flushed<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

// Hyper drops the response body only once it has queued its final framing, and
// flushes its write buffer before polling the underlying IO's flush:
// https://github.com/hyperium/hyper/blob/v1.10.1/src/proto/h1/dispatch.rs
// https://github.com/hyperium/hyper/blob/v1.10.1/src/proto/h1/io.rs
impl<I: AsyncWrite + Unpin> AsyncWrite for Flushed<I> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.exchange.flushed();
        }
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
