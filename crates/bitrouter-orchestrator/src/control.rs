//! A launch fence shared by input admission and owned worker dispatch. It is
//! held only for synchronous state changes/spawn, never database or worker I/O.
use std::sync::Mutex;

use bitrouter_ai::types::Prompt;
use tokio::sync::{mpsc, oneshot, watch};

pub(crate) struct ModelBoundary {
    pub prompt: Prompt,
    pub step_id: String,
    pub item_id: String,
    pub context_version: u64,
    pub max_bytes: usize,
    pub response: oneshot::Sender<Result<(Prompt, u64), String>>,
}

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
            if self.pending() {
                return;
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct TurnControl {
    pub fence: std::sync::Arc<LaunchFence>,
    pub models: mpsc::Sender<ModelBoundary>,
}
