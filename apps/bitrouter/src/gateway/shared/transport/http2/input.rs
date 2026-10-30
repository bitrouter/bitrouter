use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};

/// Release receive-window capacity only as the application polls DATA.
pub(super) struct Input {
    inner: h2::RecvStream,
    remaining: Option<u64>,
    data_done: bool,
    done: bool,
}

impl Input {
    pub(super) fn new(inner: h2::RecvStream, length: Option<u64>) -> Self {
        Self {
            inner,
            remaining: length,
            data_done: false,
            done: false,
        }
    }
}

// https://docs.rs/h2/latest/h2/struct.RecvStream.html#method.flow_control
impl Body for Input {
    type Data = Bytes;
    type Error = h2::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, h2::Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        if !self.data_done {
            match ready!(self.inner.poll_data(cx)) {
                Some(Ok(data)) => {
                    self.inner.flow_control().release_capacity(data.len())?;
                    self.remaining = self
                        .remaining
                        .map(|remaining| remaining.saturating_sub(data.len() as u64));
                    return Poll::Ready(Some(Ok(Frame::data(data))));
                }
                Some(Err(error)) => return Poll::Ready(Some(Err(error))),
                None => self.data_done = true,
            }
        }
        let trailers = ready!(self.inner.poll_trailers(cx));
        self.done = true;
        Poll::Ready(match trailers {
            Ok(trailers) => trailers.map(|trailers| Ok(Frame::trailers(trailers))),
            Err(error) => Some(Err(error)),
        })
    }

    fn is_end_stream(&self) -> bool {
        self.done || self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.remaining
            .map_or_else(SizeHint::new, SizeHint::with_exact)
    }
}
