use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use bytes::Bytes;
use http::HeaderMap;
use http_body::{Body as HttpBody, Frame, SizeHint};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{CHUNK_BYTES, HEADER_VALUES, IngressState, METADATA_BYTES};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Count every field value, including duplicates, without serializing a second
/// copy. Reserve space/count for Hyper's Date, framing and canonical status line.
pub(super) fn valid_metadata(headers: &HeaderMap) -> bool {
    if headers.len() > HEADER_VALUES - 4 {
        return false;
    }
    headers
        .iter()
        .try_fold(512usize, |bytes, (name, value)| {
            bytes
                .checked_add(name.as_str().len())?
                .checked_add(value.as_bytes().len())?
                .checked_add(4)
                .filter(|total| *total <= METADATA_BYTES)
        })
        .is_some()
}

/// The inner body retains application custody while an original source frame
/// or a pending remainder exists. Hyper receives separately allocated chunks,
/// including a small tail backed by a much larger producer allocation.
pub(super) struct Output {
    inner: Body,
    pending: Bytes,
    done: bool,
}

impl Output {
    pub(super) fn new(inner: Body) -> Self {
        Self {
            inner,
            pending: Bytes::new(),
            done: false,
        }
    }

    fn chunk(&mut self) -> Frame<Bytes> {
        let count = self.pending.len().min(CHUNK_BYTES);
        let copied = Bytes::copy_from_slice(&self.pending[..count]);
        if count == self.pending.len() {
            self.pending = Bytes::new();
        } else {
            // Only this wrapper retains the original backing allocation.
            self.pending = self.pending.slice(count..);
        }
        Frame::data(copied)
    }
}

// Body frame/EOS/size-hint contract:
// https://docs.rs/http-body/1.0.1/http_body/trait.Body.html
impl HttpBody for Output {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        if !self.pending.is_empty() {
            return Poll::Ready(Some(Ok(self.chunk())));
        }
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                Ok(data) => {
                    self.pending = data;
                    Poll::Ready(Some(Ok(self.chunk())))
                }
                Err(frame) => {
                    if frame
                        .trailers_ref()
                        .is_some_and(|headers| !valid_metadata(headers))
                    {
                        self.done = true;
                        Poll::Ready(Some(Err(
                            "shared response trailers exceed transport bounds".into(),
                        )))
                    } else {
                        Poll::Ready(Some(Ok(frame)))
                    }
                }
            },
            Poll::Ready(Some(Err(error))) => {
                self.done = true;
                Poll::Ready(Some(Err(error.into())))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done || (self.pending.is_empty() && self.inner.is_end_stream())
    }

    fn size_hint(&self) -> SizeHint {
        if self.done {
            return SizeHint::with_exact(0);
        }
        let inner = self.inner.size_hint();
        let pending = self.pending.len() as u64;
        let mut hint = SizeHint::new();
        if let Some(lower) = inner.lower().checked_add(pending) {
            hint.set_lower(lower);
        }
        if let Some(upper) = inner.upper().and_then(|upper| upper.checked_add(pending)) {
            hint.set_upper(upper);
        }
        hint
    }
}

pub(super) struct Ingress<B> {
    inner: B,
    remaining: usize,
    state: watch::Sender<IngressState>,
    done: bool,
}

impl<B: HttpBody + Unpin> Ingress<B> {
    pub(super) fn new(
        inner: B,
        limit: usize,
        state: watch::Sender<IngressState>,
        timeout: Duration,
    ) -> Self {
        let done = inner.is_end_stream();
        state.send_replace(if done {
            IngressState::Complete
        } else {
            IngressState::Body(Instant::now() + timeout)
        });
        Self {
            inner,
            remaining: limit,
            state,
            done,
        }
    }
}

impl<B: HttpBody<Data = Bytes> + Unpin> HttpBody for Ingress<B>
where
    B::Error: Into<Error>,
{
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let Some(remaining) = self.remaining.checked_sub(data.len()) else {
                        self.done = true;
                        return Poll::Ready(Some(Err(
                            "shared request body exceeds transport bound".into(),
                        )));
                    };
                    self.remaining = remaining;
                }
                if frame
                    .trailers_ref()
                    .is_some_and(|headers| !valid_metadata(headers))
                {
                    self.done = true;
                    return Poll::Ready(Some(Err(
                        "shared request trailers exceed transport bounds".into(),
                    )));
                }
                if self.inner.is_end_stream() {
                    self.state.send_replace(IngressState::Complete);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(None) => {
                self.done = true;
                self.state.send_replace(IngressState::Complete);
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                self.done = true;
                Poll::Ready(Some(Err(error.into())))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done || self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        if self.done {
            return SizeHint::with_exact(0);
        }
        // Collectors may reserve from hints before polling. The wrapper can
        // never yield more bytes than its remaining allowance, even when the
        // peer advertises an arbitrarily large Content-Length.
        let inner = self.inner.size_hint();
        let mut hint = SizeHint::new();
        hint.set_lower(inner.lower().min(self.remaining as u64));
        hint.set_upper(inner.upper().unwrap_or(u64::MAX).min(self.remaining as u64));
        hint
    }
}
