//! One revocable handoff, never a producer queue. Receivers carry no authority.
use super::owner::Owner;
use crate::gateway::error::{BitrouterError, Result};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
struct State<T> {
    item: Option<T>,
    closed: bool,
    receiver: bool,
}
struct Slot<T> {
    state: Mutex<State<T>>,
    changed: Notify,
}
pub(crate) struct Sender<T>(Arc<Slot<T>>);
pub(crate) struct Receiver<T>(Arc<Slot<T>>);
pub(crate) fn channel<T: Send + 'static>(owner: &Arc<Owner>) -> Result<(Sender<T>, Receiver<T>)> {
    let slot = Arc::new(Slot {
        state: Mutex::new(State {
            item: None,
            closed: false,
            receiver: true,
        }),
        changed: Notify::new(),
    });
    let weak = Arc::downgrade(&slot);
    owner.on_cancel(Box::new(move || {
        if let Some(slot) = weak.upgrade() {
            slot.close();
        }
    }))?;
    Ok((Sender(slot.clone()), Receiver(slot)))
}
impl<T> Slot<T> {
    fn close(&self) {
        let discarded = match self.state.lock() {
            Ok(mut state) => {
                state.closed = true;
                state.item.take()
            }
            Err(error) => {
                let mut state = error.into_inner();
                state.closed = true;
                state.item.take()
            }
        };
        drop(discarded);
        self.changed.notify_waiters();
    }
}
impl<T> Sender<T> {
    /// Stop polling an upstream producer when this handoff loses its receiver.
    /// This is local to the channel: consuming a preparation result does not
    /// cancel the execution now owned by the next delivery stage.
    pub(crate) async fn closed(&self) {
        loop {
            let notified = self.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.0.state.lock();
                if !state
                    .as_ref()
                    .is_ok_and(|state| !state.closed && state.receiver)
                {
                    return;
                }
            }
            notified.await;
        }
    }

    /// Return only once the receiver took the value or disappeared. Cancellation
    /// destroys an unread value outside the mutex, before capacity can drain.
    pub(crate) async fn send(&self, item: T) -> Result<()> {
        {
            let mut state = self
                .0
                .state
                .lock()
                .map_err(|_| BitrouterError::UpstreamUnavailable)?;
            if state.closed || !state.receiver || state.item.is_some() {
                return Err(BitrouterError::UpstreamUnavailable);
            }
            state.item = Some(item);
        }
        self.0.changed.notify_waiters();
        loop {
            let notified = self.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self
                    .0
                    .state
                    .lock()
                    .map_err(|_| BitrouterError::UpstreamUnavailable)?;
                if state.closed || !state.receiver {
                    return Err(BitrouterError::UpstreamUnavailable);
                }
                if state.item.is_none() {
                    return Ok(());
                }
            }
            notified.await;
        }
    }
}
impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // Normal producer completion preserves its last value until consumed.
        if let Ok(mut state) = self.0.state.lock() {
            state.closed = true;
        }
        self.0.changed.notify_waiters();
    }
}
impl<T> Receiver<T> {
    pub(crate) async fn recv(&mut self) -> Option<T> {
        loop {
            let notified = self.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (item, closed) = match self.0.state.lock() {
                Ok(mut state) => (state.item.take(), state.closed),
                Err(_) => return None,
            };
            if item.is_some() {
                self.0.changed.notify_waiters();
                return item;
            }
            if closed {
                return None;
            }
            notified.await;
        }
    }
}
impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.receiver = false;
        }
        self.0.close();
    }
}
/// Count serialization without allocating another copy or retaining metadata.
pub(crate) fn bounded_part(part: &bitrouter_ai::types::StreamPart) -> Result<()> {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("stream output overflow"))?;
            if self.0 > 64 * 1024 {
                return Err(std::io::Error::other("stream output limit"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(0), part).map_err(|_| BitrouterError::UpstreamInvalidResponse {
        message: "tenant stream output exceeds the bounded delivery slot".into(),
        usage: None,
    })
}
