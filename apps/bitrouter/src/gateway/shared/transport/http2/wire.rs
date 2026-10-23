//! Observe frame boundaries without decoding HPACK or replacing h2 validation.
//! Header reservations survive service completion until END_HEADERS is written.
//! https://httpwg.org/specs/rfc9113.html#FrameHeader

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::io::{Error, ErrorKind, IoSlice, Result};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use http::HeaderMap;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Notify;
use tokio::time::{Instant, Sleep};
use tokio_util::sync::CancellationToken;

const HEADER_BUDGET: usize = 1024 * 1024;

#[derive(Default)]
struct Pending {
    bytes: usize,
    streams: BTreeMap<u32, VecDeque<usize>>,
    cancelled: BTreeSet<u32>,
}

#[derive(Default)]
pub(super) struct Headers {
    pending: Mutex<Pending>,
    changed: Notify,
    pub(super) exhausted: CancellationToken,
}

impl Headers {
    fn lock(&self) -> MutexGuard<'_, Pending> {
        match self.pending.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub(super) fn reserve(&self, id: u32, headers: &HeaderMap) -> bool {
        // Account decoded names/values, per-field representation overhead and
        // frame/status overhead. This bounds retained metadata, not exact RSS.
        let bytes = headers.iter().fold(512, |sum, (name, value)| {
            sum + name.as_str().len() + value.as_bytes().len() + 32
        });
        let mut pending = self.lock();
        if bytes > HEADER_BUDGET.saturating_sub(pending.bytes) {
            self.exhausted.cancel();
            return false;
        }
        pending.bytes += bytes;
        pending.streams.entry(id).or_default().push_back(bytes);
        true
    }

    pub(super) fn unsent(&self, id: u32) {
        self.release(id, false, true);
    }

    pub(super) async fn released(&self, id: u32) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Register before checking the ledger; releases wake every stream.
            changed.as_mut().enable();
            if !self.lock().streams.contains_key(&id) {
                return;
            }
            changed.await;
        }
    }

    pub(super) fn cancel(&self, id: u32) {
        let mut pending = self.lock();
        // Only call after h2 has processed a reset, never on raw incoming RST.
        // h2 discards queued headers but cannot retract its encoded bytes.
        if pending.streams.contains_key(&id) {
            pending.cancelled.insert(id);
        }
    }

    fn flushed(&self) {
        let mut pending = self.lock();
        if pending.cancelled.is_empty() {
            return;
        }
        for id in std::mem::take(&mut pending.cancelled) {
            if let Some(queue) = pending.streams.remove(&id) {
                pending.bytes -= queue.into_iter().sum::<usize>();
            }
        }
        drop(pending);
        self.changed.notify_waiters();
    }

    fn release(&self, id: u32, all: bool, last: bool) {
        let mut pending = self.lock();
        let (bytes, empty) = match pending.streams.get_mut(&id) {
            Some(queue) => {
                let bytes = if all {
                    queue.drain(..).sum()
                } else if last {
                    queue.pop_back().unwrap_or(0)
                } else {
                    queue.pop_front().unwrap_or(0)
                };
                (bytes, queue.is_empty())
            }
            None => return,
        };
        pending.bytes -= bytes;
        if empty {
            pending.streams.remove(&id);
            pending.cancelled.remove(&id);
        }
        drop(pending);
        self.changed.notify_waiters();
    }
}

#[derive(Clone, Copy)]
struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
}

#[derive(Default)]
struct Frames {
    header: [u8; 9],
    used: usize,
    remaining: usize,
}

impl Frames {
    fn consume(&mut self, bytes: &[u8]) -> (usize, Option<Frame>) {
        let count = if self.used < 9 {
            let count = bytes.len().min(9 - self.used);
            self.header[self.used..self.used + count].copy_from_slice(&bytes[..count]);
            self.used += count;
            if self.used == 9 {
                self.remaining = usize::from(self.header[0]) << 16
                    | usize::from(self.header[1]) << 8
                    | usize::from(self.header[2]);
            }
            count
        } else {
            let count = bytes.len().min(self.remaining);
            self.remaining -= count;
            count
        };
        let frame = if self.used == 9 && self.remaining == 0 {
            self.used = 0;
            Some(Frame {
                kind: self.header[3],
                flags: self.header[4],
                stream: u32::from_be_bytes([
                    self.header[5],
                    self.header[6],
                    self.header[7],
                    self.header[8],
                ]) & 0x7fff_ffff,
            })
        } else {
            None
        };
        (count, frame)
    }
}

pub(super) struct Wire<I> {
    inner: I,
    headers: Arc<Headers>,
    read: Frames,
    written: Frames,
    preface: usize,
    header_block: bool,
    deadline: Option<Pin<Box<Sleep>>>,
    timeout: Duration,
}

impl<I> Wire<I> {
    pub(super) fn new(
        inner: I,
        headers: Arc<Headers>,
        initial: Instant,
        timeout: Duration,
    ) -> Self {
        Self {
            inner,
            headers,
            read: Frames::default(),
            written: Frames::default(),
            preface: 24,
            header_block: false,
            deadline: Some(Box::pin(tokio::time::sleep_until(initial))),
            timeout,
        }
    }

    fn received(&mut self, mut bytes: &[u8]) {
        let skip = bytes.len().min(self.preface);
        self.preface -= skip;
        bytes = &bytes[skip..];
        while !bytes.is_empty() {
            if self.deadline.is_none() {
                self.deadline = Some(Box::pin(tokio::time::sleep(self.timeout)));
            }
            let (count, frame) = self.read.consume(bytes);
            bytes = &bytes[count..];
            if let Some(frame) = frame {
                if frame.kind == 1 {
                    self.header_block = frame.flags & 4 == 0;
                }
                if frame.kind == 9 && frame.flags & 4 != 0 {
                    self.header_block = false;
                }
                if !self.header_block {
                    self.deadline = None;
                }
            }
        }
    }

    fn sent(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let (count, frame) = self.written.consume(bytes);
            bytes = &bytes[count..];
            if let Some(frame) = frame {
                if matches!(frame.kind, 1 | 9) && frame.flags & 4 != 0 {
                    self.headers.release(frame.stream, false, false);
                } else if frame.kind == 3 {
                    self.headers.release(frame.stream, true, false);
                }
            }
        }
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for Wire<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        if self
            .deadline
            .as_mut()
            .is_some_and(|timer| timer.as_mut().poll(cx).is_ready())
        {
            return Poll::Ready(Err(Error::new(
                ErrorKind::TimedOut,
                "HTTP/2 preface or frame/header block stalled",
            )));
        }
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            self.received(&buf.filled()[before..]);
            // Register the timer even when this read completed synchronously.
            if let Some(timer) = self.deadline.as_mut() {
                let _ = timer.as_mut().poll(cx);
            }
        }
        result
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for Wire<I> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        if let Poll::Ready(Ok(count)) = result {
            self.sent(&bytes[..count]);
        }
        result
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write_vectored(cx, buffers);
        if let Poll::Ready(Ok(mut remaining)) = result {
            for buffer in buffers {
                let count = buffer.len().min(remaining);
                self.sent(&buffer[..count]);
                remaining -= count;
                if remaining == 0 {
                    break;
                }
            }
        }
        result
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        if let Poll::Ready(Ok(())) = result {
            // h2's FramedWrite calls this only after writing its entire encoder
            // buffer, including CONTINUATIONs. Any remaining cancelled entries
            // therefore describe discarded queue items, not encoded bytes.
            // https://docs.rs/h2/0.4.15/src/h2/codec/framed_write.rs.html
            self.headers.flushed();
        }
        result
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests;
