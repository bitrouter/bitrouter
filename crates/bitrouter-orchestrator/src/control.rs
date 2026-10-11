//! A launch fence shared by input admission and owned worker dispatch. It is
//! held only for synchronous state changes/spawn, never database or worker I/O.
use std::sync::Mutex;
use tokio::sync::watch;

pub(crate) struct LaunchFence {
    sealed: Mutex<bool>,
    changed: watch::Sender<bool>,
}

impl Default for LaunchFence {
    fn default() -> Self {
        Self {
            sealed: Mutex::new(false),
            changed: watch::channel(false).0,
        }
    }
}

impl LaunchFence {
    fn lock(&self) -> std::sync::MutexGuard<'_, bool> {
        match self.sealed.lock() {
            Ok(value) => value,
            Err(error) => error.into_inner(),
        }
    }
    pub fn set(&self, value: bool) {
        let mut sealed = self.lock();
        *sealed = value;
        self.changed.send_replace(value);
    }
    pub fn pending(&self) -> bool {
        *self.lock()
    }
    pub fn launch<T>(&self, work: impl FnOnce() -> T) -> Option<T> {
        let sealed = self.lock();
        if *sealed { None } else { Some(work()) }
    }

    pub async fn received(&self) {
        let mut changed = self.changed.subscribe();
        loop {
            if self.pending() || changed.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct TurnControl {
    pub fence: std::sync::Arc<LaunchFence>,
    pub native: std::sync::Arc<NativeInputs>,
}

/// Native Thread receipt IDs are also Core steering operation IDs. Entries
/// enter this bounded queue only after the Thread receipt is durable.
#[derive(Default)]
pub(crate) struct NativeInputs {
    pending: Mutex<std::collections::VecDeque<(String, String)>>,
    changed: tokio::sync::Notify,
}

impl NativeInputs {
    pub(crate) fn push(&self, id: String, text: String) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !pending.iter().any(|(known, _)| known == &id) {
            pending.push_back((id, text));
        }
        self.changed.notify_one();
    }

    pub(crate) fn take(&self) -> std::collections::VecDeque<(String, String)> {
        std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    pub(crate) async fn changed(&self) {
        self.changed.notified().await;
    }
}
