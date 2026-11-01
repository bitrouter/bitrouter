use std::io::{IoSlice, Result as IoResult};

use tokio::io::ReadBuf;

use super::*;

pub(super) struct Vectored<I> {
    pub inner: I,
    pub enabled: bool,
}

impl<I: AsyncRead + Unpin> AsyncRead for Vectored<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<IoResult<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl<I: AsyncWrite + Unpin> AsyncWrite for Vectored<I> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<IoResult<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<IoResult<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.enabled
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Allows reads and no-op flush while writes remain blocked, matching TCP's
/// flush contract. No external wake is issued for the stalled write.
struct Fake {
    writes: Arc<AtomicUsize>,
}

impl AsyncRead for Fake {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<IoResult<()>> {
        buf.put_slice(b"x");
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for Fake {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<IoResult<usize>> {
        if self.writes.swap(0, Ordering::SeqCst) > 0 {
            Poll::Ready(Ok(buf.len()))
        } else {
            Poll::Pending
        }
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<IoResult<usize>> {
        self.poll_write(cx, bufs.first().map_or(&[], |buf| buf.as_ref()))
    }
    fn is_write_vectored(&self) -> bool {
        true
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
        Poll::Ready(Ok(()))
    }
}

fn poll_write(
    io: &mut WriteDeadline<Fake>,
    vectored: bool,
    cx: &mut Context<'_>,
) -> Poll<IoResult<usize>> {
    if vectored {
        Pin::new(io).poll_write_vectored(cx, &[IoSlice::new(b"x")])
    } else {
        Pin::new(io).poll_write(cx, b"x")
    }
}

#[tokio::test(start_paused = true)]
async fn repeated_pending_reads_and_noop_flush_do_not_extend_write_deadline() -> Result<()> {
    for vectored in [false, true] {
        let mut io = WriteDeadline::new(
            Fake {
                writes: Arc::default(),
            },
            Duration::from_secs(2),
        );
        for _ in 0..3 {
            ensure!(
                std::future::poll_fn(|cx| Poll::Ready(
                    poll_write(&mut io, vectored, cx).is_pending()
                ))
                .await
            );
            io.flush().await?;
            let mut read = [0; 1];
            io.read_exact(&mut read).await?;
            tokio::time::advance(Duration::from_millis(500)).await;
        }
        let stalled = tokio::spawn(async move {
            std::future::poll_fn(|cx| poll_write(&mut io, vectored, cx)).await
        });
        tokio::time::advance(Duration::from_millis(500)).await;
        let result = stalled.await?;
        ensure!(result.is_err_and(|error| error.kind() == std::io::ErrorKind::TimedOut));
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn positive_write_progress_resets_but_idle_body_does_not_start_timer() -> Result<()> {
    for vectored in [false, true] {
        let writes = Arc::new(AtomicUsize::new(0));
        let mut io = WriteDeadline::new(
            Fake {
                writes: writes.clone(),
            },
            Duration::from_secs(2),
        );
        tokio::time::advance(Duration::from_secs(60)).await;
        ensure!(
            std::future::poll_fn(|cx| Poll::Ready(poll_write(&mut io, vectored, cx).is_pending()))
                .await
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        writes.store(1, Ordering::SeqCst);
        ensure!(std::future::poll_fn(|cx| poll_write(&mut io, vectored, cx)).await? == 1);
        ensure!(
            std::future::poll_fn(|cx| Poll::Ready(poll_write(&mut io, vectored, cx).is_pending()))
                .await
        );
        tokio::time::advance(Duration::from_millis(1500)).await;
        ensure!(
            std::future::poll_fn(|cx| Poll::Ready(poll_write(&mut io, vectored, cx).is_pending()))
                .await
        );
        tokio::time::advance(Duration::from_millis(500)).await;
        ensure!(
            std::future::poll_fn(|cx| poll_write(&mut io, vectored, cx))
                .await
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn t2_pending_write_is_a_shutdown_barrier() -> Result<()> {
    for vectored in [false, true] {
        let mut io = WriteDeadline::new(
            Fake {
                writes: Arc::default(),
            },
            Duration::from_secs(2),
        );
        ensure!(
            std::future::poll_fn(|cx| Poll::Ready(poll_write(&mut io, vectored, cx).is_pending()))
                .await
        );
        ensure!(
            std::future::poll_fn(|cx| Poll::Ready(
                Pin::new(&mut io).poll_shutdown(cx).is_pending()
            ))
            .await,
            "shutdown skipped unresolved write"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        ensure!(
            std::future::poll_fn(|cx| Poll::Ready(
                Pin::new(&mut io).poll_shutdown(cx).is_pending()
            ))
            .await
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        ensure!(
            io.shutdown()
                .await
                .is_err_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
        );
    }
    Ok(())
}

struct FlushAndShutdown {
    flush_pending: Arc<AtomicBool>,
    shutdown_pending: bool,
}

impl AsyncWrite for FlushAndShutdown {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<IoResult<usize>> {
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
        if self.flush_pending.load(Ordering::SeqCst) {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
        if self.shutdown_pending {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

#[tokio::test(start_paused = true)]
async fn t2_pending_flush_blocks_shutdown_until_progress_or_timeout() -> Result<()> {
    for resumes in [false, true] {
        let flush_pending = Arc::new(AtomicBool::new(true));
        let mut io = WriteDeadline::new(
            FlushAndShutdown {
                flush_pending: flush_pending.clone(),
                shutdown_pending: false,
            },
            Duration::from_secs(2),
        );
        ensure!(
            std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut io).poll_flush(cx).is_pending()))
                .await
        );
        ensure!(
            std::future::poll_fn(|cx| Poll::Ready(
                Pin::new(&mut io).poll_shutdown(cx).is_pending()
            ))
            .await,
            "shutdown skipped unresolved flush"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        if resumes {
            flush_pending.store(false, Ordering::SeqCst);
            io.flush().await?;
            io.shutdown().await?;
        } else {
            tokio::time::advance(Duration::from_secs(1)).await;
            ensure!(
                io.shutdown()
                    .await
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
            );
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn t2_noop_flush_does_not_reset_inner_shutdown_deadline() -> Result<()> {
    let mut io = WriteDeadline::new(
        FlushAndShutdown {
            flush_pending: Arc::default(),
            shutdown_pending: true,
        },
        Duration::from_secs(2),
    );
    ensure!(
        std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut io).poll_shutdown(cx).is_pending()))
            .await
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    io.flush().await?;
    ensure!(
        std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut io).poll_shutdown(cx).is_pending()))
            .await
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    ensure!(
        std::future::poll_fn(|cx| Pin::new(&mut io).poll_shutdown(cx))
            .await
            .is_err_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
    );
    Ok(())
}
