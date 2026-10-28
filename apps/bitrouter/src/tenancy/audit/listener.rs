//! Finite socket ownership; no request body or unbounded idle archive clients.
use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tokio_util::sync::CancellationToken;
pub(super) struct Bounded {
    listener: TcpListener,
    slots: Arc<Semaphore>,
    stop: CancellationToken,
}
impl Bounded {
    pub(super) fn new(listener: TcpListener, stop: CancellationToken) -> Self {
        Self {
            listener,
            stop,
            slots: Arc::new(Semaphore::new(32)),
        }
    }
}
impl axum::serve::Listener for Bounded {
    type Io = Socket;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let permit = match self.slots.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    std::future::pending::<()>().await;
                    continue;
                }
            };
            match self.listener.accept().await {
                Ok((stream, addr)) => {
                    return (
                        Socket {
                            stream,
                            _permit: permit,
                            stop: Box::pin(self.stop.clone().cancelled_owned()),
                            deadline: Box::pin(tokio::time::sleep(Duration::from_secs(60))),
                        },
                        addr,
                    );
                }
                Err(_) => {
                    drop(permit);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}
pub(super) struct Socket {
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
    stop: Pin<Box<dyn Future<Output = ()> + Send>>,
    deadline: Pin<Box<tokio::time::Sleep>>,
}
impl Socket {
    fn ended(&mut self, cx: &mut Context<'_>) -> bool {
        self.stop.as_mut().poll(cx).is_ready() || self.deadline.as_mut().poll(cx).is_ready()
    }
}
impl AsyncRead for Socket {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.ended(cx) {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}
impl AsyncWrite for Socket {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.ended(cx) {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        Pin::new(&mut self.stream).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.ended(cx) {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
