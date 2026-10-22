use std::io::{IoSlice, Result};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

// Cleartext prior knowledge, not the obsolete HTTP/1 Upgrade handshake:
// https://httpwg.org/specs/rfc9113.html#preface
const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

pub(super) async fn detect<I: AsyncRead + Unpin>(mut inner: I) -> Result<(bool, Replay<I>)> {
    let mut prefix = [0; 24];
    let mut length = 0;
    let mut http2 = true;
    for expected in PREFACE {
        let byte = inner.read_u8().await?;
        prefix[length] = byte;
        length += 1;
        if byte != *expected {
            http2 = false;
            break;
        }
    }
    Ok((
        http2,
        Replay {
            inner,
            prefix,
            length,
            position: 0,
        },
    ))
}

pub(super) struct Replay<I> {
    inner: I,
    prefix: [u8; 24],
    length: usize,
    position: usize,
}

impl<I: AsyncRead + Unpin> AsyncRead for Replay<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        if self.position < self.length {
            let count = buf.remaining().min(self.length - self.position);
            buf.put_slice(&self.prefix[self.position..self.position + count]);
            self.position += count;
            Poll::Ready(Ok(()))
        } else {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for Replay<I> {
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
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
