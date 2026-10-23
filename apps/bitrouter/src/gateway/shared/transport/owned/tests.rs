use std::sync::Weak;

use axum::routing::get;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::Notify;

use super::*;

struct Held {
    dropped: Arc<AtomicBool>,
    entered: Arc<Notify>,
}

impl Future for Held {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        self.entered.notify_one();
        Poll::Pending
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

async fn held_connection() -> Result<(Transport, TcpStream, Arc<AtomicBool>)> {
    let entered = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let signal = entered.clone();
    let witness = dropped.clone();
    let router = Router::new().route(
        "/",
        get(move || {
            let held = Held {
                entered: signal.clone(),
                dropped: witness.clone(),
            };
            async move {
                held.await;
                "unreachable"
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let owner = Transport::start(listener, router, super::super::tests::limits())?;
    let mut peer = TcpStream::connect(address).await?;
    peer.write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\n\r\n")
        .await?;
    tokio::time::timeout(Duration::from_secs(5), entered.notified()).await?;
    Ok((owner, peer, dropped))
}

#[tokio::test]
async fn empty_graceful_shutdown_is_clean_and_repeatable() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let owner = Transport::start(listener, Router::new(), super::super::tests::limits())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let report = owner.drain_until(deadline).await?;
    ensure!(report.is_clean() && report.connections.started == 0);
    ensure!(owner.drain_until(deadline).await?.is_clean());
    Ok(())
}

#[tokio::test]
async fn completed_connections_remain_in_the_cached_shutdown_report() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let router = Router::new().route("/", get(|| async { "completed" }));
    let owner = Transport::start(listener, router, super::super::tests::limits())?;
    let mut peer = TcpStream::connect(address).await?;
    peer.write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\n\r\n")
        .await?;
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), peer.read_to_end(&mut response)).await??;
    ensure!(response.starts_with(b"HTTP/1.1 200") && response.ends_with(b"completed"));
    let report = owner
        .drain_until(Instant::now() + Duration::from_secs(5))
        .await?;
    ensure!(report.is_clean() && report.connections.started == 1 && report.connections.joined == 1);
    Ok(())
}

#[tokio::test]
async fn cancelled_graceful_wait_preserves_roots_and_force_is_available() -> Result<()> {
    let (owner, _peer, dropped) = held_connection().await?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut waiter = Box::pin(owner.drain_until(deadline));
    ensure!(futures::poll!(waiter.as_mut()).is_pending());
    ensure!(owner.drain_until(deadline).await.is_err());
    ensure!(
        owner
            .drain_until(deadline + Duration::from_secs(1))
            .await
            .is_err()
    );
    // Signalling never waits for the active drain waiter to release its guard.
    owner.request_force();
    ensure!(owner.report().forced && owner.report().connections.pending == 1);
    drop(waiter);
    ensure!(owner.drain_until(deadline).await.is_err());
    let report = owner.join_forced().await?;
    ensure!(report.is_quiescent() && !report.is_clean());
    ensure!(report.connections.started == 1 && report.connections.joined == 1);
    ensure!(report.connections.cancelled == 1 && dropped.load(Ordering::Acquire));
    ensure!(owner.join_forced().await?.connections.joined == 1);
    Ok(())
}

#[tokio::test]
async fn expired_grace_retains_connection_and_cannot_be_extended() -> Result<()> {
    let (owner, _peer, dropped) = held_connection().await?;
    let deadline = Instant::now() + Duration::from_millis(20);
    let report = owner.drain_until(deadline).await?;
    ensure!(report.driver.joined && report.connections.pending == 1 && !report.forced);
    ensure!(!dropped.load(Ordering::Acquire));
    let retry =
        tokio::time::timeout(Duration::from_millis(50), owner.drain_until(deadline)).await??;
    ensure!(!retry.is_quiescent() && !retry.forced);
    ensure!(
        owner
            .drain_until(Instant::now() + Duration::from_secs(5))
            .await
            .is_err()
    );
    owner.request_force();
    ensure!(owner.join_forced().await?.is_quiescent());
    ensure!(dropped.load(Ordering::Acquire));
    Ok(())
}

#[tokio::test]
async fn accept_cancellation_does_not_destroy_its_connection_set() -> Result<()> {
    let (owner, _peer, dropped) = held_connection().await?;
    if let Driver::Running(task) = &*owner
        .driver
        .lock()
        .unwrap_or_else(|error| error.into_inner())
    {
        task.abort();
    }
    let driver = owner.wait_driver().await?;
    ensure!(driver.joined && driver.cancelled);
    ensure!(owner.report().connections.pending == 1 && !owner.report().is_quiescent());
    ensure!(!dropped.load(Ordering::Acquire));
    owner.request_force();
    let report = owner.join_forced().await?;
    ensure!(report.is_quiescent() && !report.is_clean());
    ensure!(dropped.load(Ordering::Acquire));
    Ok(())
}

#[tokio::test]
async fn accepted_socket_cannot_register_after_the_force_gate() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let _peer = TcpStream::connect(listener.local_addr()?).await?;
    let (accepted, _) = listener.accept().await?;
    // The accepted socket is deliberately held outside the insertion gate.
    let owner = Transport::start(listener, Router::new(), super::super::tests::limits())?;
    owner.request_force();
    owner
        .connections
        .register(accepted, Router::new(), super::super::tests::limits());
    let report = owner.join_forced().await?;
    ensure!(report.is_quiescent() && report.connections.started == 0);
    ensure!(owner.connections.slots.available_permits() == 1);
    Ok(())
}

#[tokio::test]
async fn cancelled_force_wait_keeps_handles_before_they_are_polled() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let owner = Transport::start(listener, Router::new(), super::super::tests::limits())?;
    let dropped = Arc::new(AtomicBool::new(false));
    {
        let mut state = owner.connections.lock();
        state.tasks.spawn(Held {
            dropped: dropped.clone(),
            entered: Arc::new(Notify::new()),
        });
        state.report.started += 1;
    }
    // The current-thread runtime has not polled either spawned task yet.
    owner.request_force();
    let mut waiter = Box::pin(owner.join_forced());
    ensure!(futures::poll!(waiter.as_mut()).is_pending());
    ensure!(owner.join_forced().await.is_err());
    drop(waiter);
    ensure!(owner.report().connections.pending == 1);
    let report = owner.join_forced().await?;
    ensure!(report.is_quiescent() && report.connections.cancelled == 1);
    ensure!(dropped.load(Ordering::Acquire));
    Ok(())
}

struct PanickingPayload {
    connections: Weak<Connections>,
    unlocked: Arc<AtomicBool>,
}

impl Drop for PanickingPayload {
    fn drop(&mut self) {
        if let Some(connections) = self.connections.upgrade() {
            self.unlocked
                .store(connections.state.try_lock().is_ok(), Ordering::Release);
        }
        std::panic::resume_unwind(Box::new("accept reaper fixture panic"));
    }
}

#[tokio::test]
async fn accept_reaper_panic_preserves_other_children_and_failure_history() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let owner = Transport::start(listener, Router::new(), super::super::tests::limits())?;
    let dropped = Arc::new(AtomicBool::new(false));
    let unlocked = Arc::new(AtomicBool::new(false));
    let payload = PanickingPayload {
        connections: Arc::downgrade(&owner.connections),
        unlocked: unlocked.clone(),
    };
    {
        let mut state = owner.connections.lock();
        state.tasks.spawn(Held {
            dropped: dropped.clone(),
            entered: Arc::new(Notify::new()),
        });
        state.tasks.spawn(async move {
            std::panic::resume_unwind(Box::new(payload));
        });
        state.report.started += 2;
    }
    let driver = tokio::time::timeout(Duration::from_secs(5), owner.wait_driver()).await??;
    ensure!(driver.panicked && unlocked.load(Ordering::Acquire));
    ensure!(owner.report().connections.pending == 1 && !dropped.load(Ordering::Acquire));
    owner.request_force();
    let report = owner.join_forced().await?;
    ensure!(report.is_quiescent() && !report.is_clean());
    ensure!(report.connections.joined == 2 && report.connections.panicked == 1);
    ensure!(dropped.load(Ordering::Acquire));
    Ok(())
}

struct InspectingPayload {
    owner: Weak<Transport>,
    observed_barrier: Arc<AtomicBool>,
}

impl Drop for InspectingPayload {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            let report = owner.report();
            self.observed_barrier.store(
                report.driver.joined && report.connections.pending == 0 && !report.is_quiescent(),
                Ordering::Release,
            );
        }
    }
}

#[tokio::test]
async fn connection_panic_payload_destruction_is_part_of_the_join_barrier() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let owner = Arc::new(Transport::start(
        listener,
        Router::new(),
        super::super::tests::limits(),
    )?);
    let observed = Arc::new(AtomicBool::new(false));
    let payload = InspectingPayload {
        owner: Arc::downgrade(&owner),
        observed_barrier: observed.clone(),
    };
    {
        let mut state = owner.connections.lock();
        state.tasks.spawn(async move {
            std::panic::resume_unwind(Box::new(payload));
        });
        state.report.started += 1;
    }
    // Close before either root is polled, so the driver exits without reaping.
    let report = owner
        .drain_until(Instant::now() + Duration::from_secs(5))
        .await?;
    ensure!(observed.load(Ordering::Acquire));
    ensure!(report.is_quiescent() && !report.is_clean() && report.connections.panicked == 1);
    Ok(())
}

#[tokio::test]
async fn accept_panic_payload_destruction_is_part_of_the_join_barrier() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let owner = Arc::new(Transport::start(
        listener,
        Router::new(),
        super::super::tests::limits(),
    )?);
    let deadline = Instant::now() + Duration::from_secs(5);
    ensure!(owner.drain_until(deadline).await?.is_clean());
    let observed = Arc::new(AtomicBool::new(false));
    let payload = InspectingPayload {
        owner: Arc::downgrade(&owner),
        observed_barrier: observed.clone(),
    };
    // Substitute a controlled failing driver only after the real driver joined.
    let task = tokio::spawn(async move {
        std::panic::resume_unwind(Box::new(payload));
    });
    *owner
        .driver
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Driver::Running(task);
    let report = owner.drain_until(deadline).await?;
    ensure!(observed.load(Ordering::Acquire));
    ensure!(report.is_quiescent() && !report.is_clean() && report.driver.panicked);
    Ok(())
}
