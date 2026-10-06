//! Bound JSON-RPC ingress frames and SDK notification buffering.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

pub(super) struct Flow {
    permits: Arc<Semaphore>,
    queued: Mutex<VecDeque<OwnedSemaphorePermit>>,
    replies: Mutex<HashMap<String, VecDeque<OwnedSemaphorePermit>>>,
    closed: CancellationToken,
}

impl Flow {
    pub(super) fn new(bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(bytes)),
            queued: Mutex::new(VecDeque::new()),
            replies: Mutex::new(HashMap::new()),
            closed: CancellationToken::new(),
        })
    }

    pub(super) async fn reserve(
        &self,
        bytes: usize,
    ) -> Result<OwnedSemaphorePermit, agent_client_protocol::Error> {
        let bytes =
            u32::try_from(bytes).map_err(|_| super::wire::invalid("ACP update is too large"))?;
        let result = tokio::select! {
            _ = self.closed.cancelled() => Err(super::wire::invalid("ACP transport closed")),
            permit = tokio::time::timeout(std::time::Duration::from_secs(5), self.permits.clone().acquire_many_owned(bytes)) => {
                permit.map_err(|_| super::wire::invalid("ACP output stalled; reconnect to resynchronize"))?
                    .map_err(|_| super::wire::invalid("ACP transport closed"))
            }
        };
        if result.is_err() {
            self.close();
        }
        result
    }

    pub(super) fn enqueue(
        &self,
        permit: OwnedSemaphorePermit,
        send: impl FnOnce() -> Result<(), agent_client_protocol::Error>,
    ) -> Result<(), agent_client_protocol::Error> {
        let mut queue = self.queued.lock().unwrap_or_else(|e| e.into_inner());
        queue.push_back(permit);
        if let Err(error) = send() {
            queue.pop_back();
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn close(&self) {
        self.closed.cancel();
        self.queued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub(super) async fn respond(
        &self,
        responder: agent_client_protocol::Responder<serde_json::Value>,
        result: Result<serde_json::Value, agent_client_protocol::Error>,
    ) -> Result<(), agent_client_protocol::Error> {
        let size = match &result {
            Ok(value) => serde_json::to_vec(value),
            Err(error) => serde_json::to_vec(error),
        }
        .map_err(|error| super::wire::invalid(error.to_string()))?
        .len()
        .saturating_add(512);
        let permit = self.reserve(size).await?;
        let id = serde_json::to_string(responder.id())
            .map_err(|error| super::wire::invalid(error.to_string()))?;
        let mut replies = self.replies.lock().unwrap_or_else(|e| e.into_inner());
        replies.entry(id.clone()).or_default().push_back(permit);
        let sent = match result {
            Ok(value) => responder.respond(value),
            Err(error) => responder.respond_with_error(error),
        };
        if sent.is_err()
            && let Some(queue) = replies.get_mut(&id)
        {
            queue.pop_back();
            if queue.is_empty() {
                replies.remove(&id);
            }
        }
        sent
    }

    fn written(&self, value: &serde_json::Value) {
        if let Some(batch) = value.as_array() {
            for value in batch {
                self.written(value);
            }
        } else if value.get("method").and_then(serde_json::Value::as_str) == Some("session/update")
        {
            self.queued
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front();
        } else if (value.get("result").is_some() || value.get("error").is_some())
            && let Some(id) = value.get("id")
        {
            let id = id.to_string();
            let mut replies = self.replies.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(queue) = replies.get_mut(&id) {
                queue.pop_front();
                if queue.is_empty() {
                    replies.remove(&id);
                }
            }
        }
    }

    pub(super) async fn cancelled(&self) {
        self.closed.cancelled().await;
    }
}

pub(super) struct BoundedRead<R> {
    pub(super) inner: R,
    pub(super) limit: usize,
    pub(super) bytes: usize,
}

impl<R: AsyncRead + Unpin> AsyncRead for BoundedRead<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                for byte in &buf.filled()[before..] {
                    this.bytes += 1;
                    if this.bytes > this.limit {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "ACP frame exceeds request bound",
                        )));
                    }
                    if *byte == b'\n' {
                        this.bytes = 0;
                    }
                }
                Poll::Ready(Ok(()))
            }
            result => result,
        }
    }
}

pub(super) struct Output<W> {
    pub(super) inner: W,
    pub(super) flow: Arc<Flow>,
    pub(super) frame: Vec<u8>,
    pub(super) limit: usize,
    pub(super) stalled: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<W> Output<W> {
    fn stalled(&mut self, cx: &mut Context<'_>) -> bool {
        let timer = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(std::time::Duration::from_secs(5))));
        if timer.as_mut().poll(cx).is_ready() {
            self.flow.close();
            true
        } else {
            false
        }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Output<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write(cx, bytes) {
            Poll::Ready(Ok(count)) => {
                this.stalled = None;
                for byte in &bytes[..count] {
                    if this.frame.len() >= this.limit {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "ACP output frame exceeds byte bound",
                        )));
                    }
                    this.frame.push(*byte);
                    if *byte == b'\n' {
                        let value: serde_json::Value =
                            serde_json::from_slice(&this.frame).map_err(io::Error::other)?;
                        this.flow.written(&value);
                        this.frame.clear();
                    }
                }
                Poll::Ready(Ok(count))
            }
            Poll::Pending if this.stalled(cx) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ACP output stalled; reconnect to resynchronize",
            ))),
            result => result,
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Pending if this.stalled(cx) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ACP flush stalled",
            ))),
            Poll::Ready(result) => {
                this.stalled = None;
                Poll::Ready(result)
            }
            result => result,
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
