//! Process-local terminal evidence retained across cancelled finalizer waits.
//!
//! This records the sender's committed outcome, never a durable claim that a
//! remote client received bytes after a process crash.
//! <https://docs.rs/tokio/latest/tokio/sync/watch/index.html>

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pending,
    Success,
    Failed,
}

#[derive(Clone)]
pub(crate) struct Witness {
    receiver: tokio::sync::watch::Receiver<Outcome>,
}

pub(crate) struct Writer {
    sender: tokio::sync::watch::Sender<Outcome>,
}

pub(crate) fn pair() -> (Witness, Writer) {
    let (sender, receiver) = tokio::sync::watch::channel(Outcome::Pending);
    (Witness { receiver }, Writer { sender })
}

impl Witness {
    pub fn outcome(&self) -> Option<bool> {
        match *self.receiver.borrow() {
            Outcome::Pending => None,
            Outcome::Success => Some(true),
            Outcome::Failed => Some(false),
        }
    }

    /// A cancelled observer leaves both the channel value and other observers
    /// intact. It cannot consume evidence needed by the generation owner.
    pub async fn wait(&self) -> bool {
        let mut receiver = self.receiver.clone();
        loop {
            let observed = *receiver.borrow_and_update();
            match observed {
                Outcome::Success => return true,
                Outcome::Failed => return false,
                Outcome::Pending => {}
            }
            if receiver.changed().await.is_err() {
                return false;
            }
        }
    }
}

impl Writer {
    /// Called only after authoritative accounting and terminal send succeed in
    /// the same final poll. Failure paths may freeze a negative outcome earlier.
    pub fn complete(&self, successful: bool) {
        self.sender.send_if_modified(|outcome| {
            if *outcome != Outcome::Pending {
                return false;
            }
            *outcome = if successful {
                Outcome::Success
            } else {
                Outcome::Failed
            };
            true
        });
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.complete(false);
    }
}
