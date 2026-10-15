use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Both WebSocket halves must lose the actual transport on an interrupted write.
/// A closed-sender flag alone leaves tungstenite's retained frames writable by
/// the receiver's automatic pong/close replies.
#[derive(Clone)]
pub(super) struct Transport(Arc<State>);

struct State {
    io: Mutex<Option<reqwest::Upgraded>>,
    write_closed: AtomicBool,
    reader: AtomicWaker,
    writer: AtomicWaker,
}

impl Transport {
    pub(super) fn new(io: reqwest::Upgraded) -> Self {
        Self(Arc::new(State {
            io: Mutex::new(Some(io)),
            write_closed: AtomicBool::new(false),
            reader: AtomicWaker::new(),
            writer: AtomicWaker::new(),
        }))
    }

    pub(super) fn write_closed(&self) -> bool {
        self.0.write_closed.load(Ordering::Relaxed)
    }

    pub(super) fn close_writes(&self) {
        self.0.write_closed.store(true, Ordering::Relaxed);
    }

    pub(super) fn invalidated(&self) -> bool {
        self.io().is_none()
    }

    pub(super) fn invalidate(&self) {
        self.close_writes();
        // Synchronously drop the connection before waking either pending half.
        // The lock protects only synchronous polls, never an await.
        self.io().take();
        self.0.reader.wake();
        self.0.writer.wake();
    }

    pub(super) fn invalidate_on_drop(&self) -> InvalidateOnDrop<'_> {
        InvalidateOnDrop {
            transport: self,
            armed: true,
        }
    }

    fn io(&self) -> MutexGuard<'_, Option<reqwest::Upgraded>> {
        match self.0.io.lock() {
            Ok(io) => io,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

pub(super) struct InvalidateOnDrop<'a> {
    transport: &'a Transport,
    armed: bool,
}

impl InvalidateOnDrop<'_> {
    pub(super) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for InvalidateOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.transport.invalidate();
        }
    }
}

fn closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "core channel transport invalidated",
    )
}

// Register before observing the shared state so invalidation cannot strand a
// reader/writer that already returned Pending. Each poll holds the IO mutex
// only for the underlying nonblocking operation.
// https://docs.rs/futures-util/latest/futures_util/task/struct.AtomicWaker.html
// https://docs.rs/tokio/latest/tokio/io/trait.AsyncRead.html#tymethod.poll_read
impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.0.reader.register(cx.waker());
        match self.io().as_mut() {
            Some(io) => Pin::new(io).poll_read(cx, buf),
            None => Poll::Ready(Err(closed())),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.writer.register(cx.waker());
        match self.io().as_mut() {
            Some(io) => Pin::new(io).poll_write(cx, buf),
            None => Poll::Ready(Err(closed())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.writer.register(cx.waker());
        match self.io().as_mut() {
            Some(io) => Pin::new(io).poll_flush(cx),
            None => Poll::Ready(Err(closed())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.writer.register(cx.waker());
        match self.io().as_mut() {
            Some(io) => Pin::new(io).poll_shutdown(cx),
            None => Poll::Ready(Err(closed())),
        }
    }
}
