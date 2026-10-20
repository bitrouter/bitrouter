use std::future::Future;
use std::io::{Error, ErrorKind, IoSlice, Result};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Sleep;

/// Timers start on actual output backpressure. Body/service Pending does not
/// start one; incoming reads cannot prolong a blocked write. A successful no-op
/// TCP flush cannot clear a pending write's timer.
pub(super) struct WriteDeadline<I> {
    inner: I,
    timeout: Duration,
    write: Option<Pin<Box<Sleep>>>,
    flush: Option<Pin<Box<Sleep>>>,
    shutdown: Option<Pin<Box<Sleep>>>,
}

impl<I> WriteDeadline<I> {
    pub(super) fn new(inner: I, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            write: None,
            flush: None,
            shutdown: None,
        }
    }
}

fn expired(timer: &mut Option<Pin<Box<Sleep>>>, cx: &mut Context<'_>) -> bool {
    timer
        .as_mut()
        .is_some_and(|timer| timer.as_mut().poll(cx).is_ready())
}

fn pending(
    timer: &mut Option<Pin<Box<Sleep>>>,
    timeout: Duration,
    cx: &mut Context<'_>,
) -> Poll<Result<usize>> {
    let timer = timer.get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
    if timer.as_mut().poll(cx).is_ready() {
        Poll::Ready(Err(timed_out()))
    } else {
        Poll::Pending
    }
}

fn timed_out() -> Error {
    Error::new(ErrorKind::TimedOut, "shared HTTP output stalled")
}

impl<I: AsyncRead + Unpin> AsyncRead for WriteDeadline<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

// AsyncWrite progress and vectored I/O contract:
// https://docs.rs/tokio/latest/tokio/io/trait.AsyncWrite.html
impl<I: AsyncWrite + Unpin> AsyncWrite for WriteDeadline<I> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        if expired(&mut self.write, cx) {
            return Poll::Ready(Err(timed_out()));
        }
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Pending => {
                let timeout = self.timeout;
                pending(&mut self.write, timeout, cx)
            }
            result => {
                if matches!(result, Poll::Ready(Ok(count)) if count > 0) {
                    self.write = None;
                }
                result
            }
        }
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        if expired(&mut self.write, cx) {
            return Poll::Ready(Err(timed_out()));
        }
        match Pin::new(&mut self.inner).poll_write_vectored(cx, bufs) {
            Poll::Pending => {
                let timeout = self.timeout;
                pending(&mut self.write, timeout, cx)
            }
            result => {
                if matches!(result, Poll::Ready(Ok(count)) if count > 0) {
                    self.write = None;
                }
                result
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        if expired(&mut self.write, cx) || expired(&mut self.flush, cx) {
            return Poll::Ready(Err(timed_out()));
        }
        match Pin::new(&mut self.inner).poll_flush(cx) {
            Poll::Pending => {
                let timeout = self.timeout;
                pending(&mut self.flush, timeout, cx).map_ok(|_| ())
            }
            result => {
                self.flush = None;
                result
            }
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        if expired(&mut self.write, cx)
            || expired(&mut self.flush, cx)
            || expired(&mut self.shutdown, cx)
        {
            return Poll::Ready(Err(timed_out()));
        }
        // Hyper may call IO shutdown after its own buffered flush returned
        // Pending. TCP shutdown cannot flush bytes still owned by Hyper:
        // https://github.com/hyperium/hyper/blob/v1.10.1/src/proto/h1/dispatch.rs
        // Keep the connection alive for its next flush poll (or timeout).
        if self.write.is_some() || self.flush.is_some() {
            return Poll::Pending;
        }
        match Pin::new(&mut self.inner).poll_shutdown(cx) {
            Poll::Pending => {
                let timeout = self.timeout;
                pending(&mut self.shutdown, timeout, cx).map_ok(|_| ())
            }
            result => {
                self.shutdown = None;
                result
            }
        }
    }
}
