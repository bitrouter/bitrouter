//! A terminal reader whose lifetime ends before terminal custody changes.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossterm::event::{self, Event};
use tokio::sync::mpsc;

/// Own the reader, rather than leaving an asynchronous stream's worker alive
/// while an editor or a resumed view acquires the same terminal.
pub(crate) struct TerminalInput {
    receiver: mpsc::Receiver<io::Result<Event>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl TerminalInput {
    pub(crate) fn new() -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel(64);
        let stopped = Arc::new(AtomicBool::new(false));
        let stopping = stopped.clone();
        let worker = thread::Builder::new()
            .name("bitrouter-terminal-input".into())
            .spawn(move || {
                while !stopping.load(Ordering::Acquire) {
                    // Only this thread calls poll/read; there is no shared
                    // stream waker left active across terminal handoffs.
                    // https://docs.rs/crossterm/0.28.1/crossterm/event/fn.poll.html
                    let next = match event::poll(Duration::from_millis(20)) {
                        Ok(false) => continue,
                        Ok(true) if stopping.load(Ordering::Acquire) => break,
                        Ok(true) => event::read(),
                        Err(error) => Err(error),
                    };
                    let failed = next.is_err();
                    if sender.blocking_send(next).is_err() || failed {
                        break;
                    }
                }
            })?;
        Ok(Self {
            receiver,
            stopped,
            worker: Some(worker),
        })
    }

    pub(crate) async fn next(&mut self) -> Option<io::Result<Event>> {
        self.receiver.recv().await
    }

    /// Refuse a custody handoff until the old reader has exited. An unhealthy
    /// terminal/backend must not block the async runtime or shutdown forever.
    pub(crate) async fn stop(events: &mut Option<Self>) -> io::Result<()> {
        let Some(mut input) = events.take() else {
            return Ok(());
        };
        input.cancel();
        let Some(worker) = input.worker.take() else {
            return Ok(());
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !worker.is_finished() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "terminal input did not stop"))?;
        worker
            .join()
            .map_err(|_| io::Error::other("terminal input worker panicked"))
    }

    fn cancel(&mut self) {
        self.stopped.store(true, Ordering::Release);
        // Unblock a full channel before waiting for the reader.
        self.receiver.close();
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        // Cancellation/error fallback only. Explicit handoffs await stop;
        // never join a potentially stuck OS reader from a destructor.
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_releases_a_reader_blocked_on_the_bounded_queue() -> io::Result<()> {
        let (sender, receiver) = mpsc::channel(1);
        let (ready, entered) = tokio::sync::oneshot::channel();
        let worker = thread::spawn(move || {
            if sender.blocking_send(Ok(Event::FocusGained)).is_ok() {
                if ready.send(()).is_err() {
                    return;
                }
                assert!(sender.blocking_send(Ok(Event::FocusLost)).is_err());
            }
        });
        entered.await.map_err(io::Error::other)?;
        let mut input = Some(TerminalInput {
            receiver,
            stopped: Arc::new(AtomicBool::new(false)),
            worker: Some(worker),
        });
        TerminalInput::stop(&mut input).await?;
        assert!(input.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn stuck_reader_refuses_handoff_without_blocking_the_runtime() -> io::Result<()> {
        let (_sender, receiver) = mpsc::channel(1);
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let (done, finished) = tokio::sync::oneshot::channel();
        let worker = thread::spawn(move || {
            // Bound the fixture even if the implementation regresses to join.
            let _deadline = blocked.recv_timeout(Duration::from_secs(3));
            assert!(done.send(()).is_ok());
        });
        let mut input = Some(TerminalInput {
            receiver,
            stopped: Arc::new(AtomicBool::new(false)),
            worker: Some(worker),
        });
        let result =
            tokio::time::timeout(Duration::from_secs(2), TerminalInput::stop(&mut input)).await;
        // Release the fixture before assertions, including on a failed stop.
        let _released = release.send(());
        finished.await.map_err(io::Error::other)?;
        assert!(result.is_ok_and(|result| {
            result.is_err_and(|error| error.kind() == io::ErrorKind::TimedOut)
        }));
        assert!(input.is_none());
        Ok(())
    }
}
