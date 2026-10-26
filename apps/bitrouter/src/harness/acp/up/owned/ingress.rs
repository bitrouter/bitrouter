//! Raw child input admission before the pinned SDK's lines/JSON/unbounded queues.
//!
//! These are lifetime allowances, never refilled after a frame or consumer ACK.
//! They bound aggregate wire input even while capture is waiting for its writer.
//! JSON batches can contain multiple messages; cumulative bytes bound those too.
use super::Control;
use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, ReadBuf};

pub(crate) const MAX_FRAME_BYTES: usize = 64 * 1024;
pub(crate) const MAX_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_FRAMES: usize = 512;
pub(crate) const READ_CHUNK: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Limit {
    Frame = 1,
    Bytes = 2,
    Frames = 3,
}

#[derive(Clone, Copy, Default)]
struct Allowance {
    bytes: usize,
    frames: usize,
    line: usize,
    largest: usize,
}
impl Allowance {
    fn admit(mut self, input: &[u8]) -> Result<Self, Limit> {
        self.bytes = self
            .bytes
            .checked_add(input.len())
            .filter(|bytes| *bytes <= MAX_BYTES)
            .ok_or(Limit::Bytes)?;
        for byte in input {
            if self.frames >= MAX_FRAMES {
                return Err(Limit::Frames);
            }
            self.line += 1;
            if self.line > MAX_FRAME_BYTES {
                return Err(Limit::Frame);
            }
            self.largest = self.largest.max(self.line);
            if *byte == b'\n' {
                self.frames += 1;
                self.line = 0;
            }
        }
        Ok(self)
    }
}

/// No pump/task/queue is added: this reader stays inside the existing transport
/// future and its actual child owner. A rejected chunk never reaches the SDK.
pub(super) struct Ingress<R> {
    reader: R,
    control: Arc<Control>,
    allowance: Allowance,
    failed: bool,
}
impl<R> Ingress<R> {
    pub(super) fn new(reader: R, control: Arc<Control>) -> Self {
        Self {
            reader,
            control,
            allowance: Allowance::default(),
            failed: false,
        }
    }
}

fn rejected() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "owned ACP raw ingress allowance exhausted",
    )
}

impl<R: AsyncRead + Unpin> AsyncRead for Ingress<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.failed {
            return Poll::Ready(Err(rejected()));
        }
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        // Fixed scratch also bounds overshoot used to detect a missing newline.
        // No child-controlled allocation or JSON parsing precedes admission.
        let mut scratch = [0_u8; READ_CHUNK];
        let count = output.remaining().min(scratch.len());
        let mut read = ReadBuf::new(&mut scratch[..count]);
        match Pin::new(&mut this.reader).poll_read(cx, &mut read) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {}
        }
        let bytes = read.filled();
        #[cfg(test)]
        this.control
            .ingress
            .read
            .fetch_add(bytes.len(), std::sync::atomic::Ordering::Relaxed);
        match this.allowance.admit(bytes) {
            Ok(next) => {
                this.allowance = next;
                #[cfg(test)]
                this.control.ingress.admitted(next);
                output.put_slice(bytes);
                Poll::Ready(Ok(()))
            }
            Err(limit) => {
                this.failed = true;
                #[cfg(test)]
                this.control
                    .ingress
                    .rejection_boundary
                    .get_or_init(|| RejectionBoundary {
                        at: tokio::time::Instant::now(),
                        host_unknown: this
                            .control
                            .binding
                            .service()
                            .host_cleanup_unknown_site_for_test(),
                    });
                // Reuse the opener's existing stop and the same reaper. New
                // effects fail their checked gate; terminal capture still drains.
                this.control.binding.reject_ingress();
                this.control.stop.cancel();
                #[cfg(test)]
                this.control.ingress.reject(limit);
                #[cfg(not(test))]
                let _reason = limit;
                Poll::Ready(Err(rejected()))
            }
        }
    }
}

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Observes the real reader only; it cannot change admission or mint authority.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Statistics {
    read: AtomicUsize,
    bytes: AtomicUsize,
    frames: AtomicUsize,
    largest: AtomicUsize,
    reason: AtomicUsize,
    rejection_boundary: std::sync::OnceLock<RejectionBoundary>,
    reaper_failure: std::sync::OnceLock<ReaperFailure>,
    rejected: tokio_util::sync::CancellationToken,
}
// Fixed source literals and typed OS codes only; no child text or error Display.
#[cfg(test)]
pub(crate) type ReaperFailure = (
    &'static str,
    &'static str,
    Option<std::io::ErrorKind>,
    Option<i32>,
);

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RejectionBoundary {
    pub(crate) at: tokio::time::Instant,
    pub(crate) host_unknown: Option<(&'static std::panic::Location<'static>, tokio::time::Instant)>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) read: usize,
    pub(crate) bytes: usize,
    pub(crate) frames: usize,
    pub(crate) largest: usize,
    pub(crate) reason: Option<Limit>,
}
#[cfg(test)]
impl Statistics {
    pub(super) fn record_reaper_failure(
        &self,
        phase: &'static str,
        result: &std::thread::Result<anyhow::Result<()>>,
    ) {
        let (category, kind, errno) = match result {
            Err(_) => ("unwind", None, None),
            Ok(Err(error)) => {
                if let Some(error) = error.downcast_ref::<std::io::Error>() {
                    ("io", Some(error.kind()), error.raw_os_error())
                } else if let Some(error) = error.downcast_ref::<rustix::io::Errno>() {
                    ("rustix_errno", None, Some(error.raw_os_error()))
                } else if let Some(error) = error.downcast_ref::<tokio::task::JoinError>() {
                    let category = if error.is_cancelled() {
                        "join_cancelled"
                    } else if error.is_panic() {
                        "join_panicked"
                    } else {
                        "join_other"
                    };
                    (category, None, None)
                } else {
                    ("other_typed_error", None, None)
                }
            }
            Ok(Ok(())) => return,
        };
        let _recorded = self.reaper_failure.set((phase, category, kind, errno));
    }
    pub(crate) fn reaper_failure(&self) -> Option<ReaperFailure> {
        self.reaper_failure.get().copied()
    }
    fn admitted(&self, next: Allowance) {
        self.bytes.store(next.bytes, Ordering::Release);
        self.frames.store(next.frames, Ordering::Release);
        self.largest.store(next.largest, Ordering::Release);
    }
    fn reject(&self, limit: Limit) {
        self.reason.store(limit as usize, Ordering::Release);
        self.rejected.cancel();
    }
    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot {
            read: self.read.load(Ordering::Acquire),
            bytes: self.bytes.load(Ordering::Acquire),
            frames: self.frames.load(Ordering::Acquire),
            largest: self.largest.load(Ordering::Acquire),
            reason: match self.reason.load(Ordering::Acquire) {
                1 => Some(Limit::Frame),
                2 => Some(Limit::Bytes),
                3 => Some(Limit::Frames),
                _ => None,
            },
        }
    }
    pub(crate) fn rejection_boundary(&self) -> Option<RejectionBoundary> {
        self.rejection_boundary.get().copied()
    }
    pub(crate) async fn rejected(&self) {
        self.rejected.cancelled().await;
    }
}
