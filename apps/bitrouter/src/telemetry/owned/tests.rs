use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Context as _;

use super::*;

fn configuration(metrics: bool) -> OtelConfig {
    let mut config = OtelConfig::default();
    config.metrics.enabled = metrics;
    config.service_name = "shared-owned-fixture".into();
    config
}

#[tokio::test]
async fn graceful_shutdown_joins_the_real_sdk_worker_and_caches_outcome() -> Result<()> {
    let owner = ProcessExporter::new(configuration(false))?;
    assert_eq!(
        owner.exporter().status().service_name.as_deref(),
        Some("shared-owned-fixture")
    );
    assert_eq!(owner.workers.lock().tasks.len(), 1);
    let first = owner
        .drain_until(Instant::now() + Duration::from_secs(5))
        .await?;
    ensure!(first.is_clean() && first.workers.joined == 1);
    let outcome = first.exporter.context("shutdown outcome absent")?;
    let second = owner
        .drain_until(Instant::now() - Duration::from_secs(1))
        .await?;
    ensure!(second.is_clean() && second.workers.joined == 1);
    ensure!(second.exporter == Some(outcome));
    ensure!(owner.force_join().await?.is_clean());
    Ok(())
}

#[tokio::test]
async fn force_joins_both_sdk_processors_and_reports_the_discarded_work() -> Result<()> {
    let owner = ProcessExporter::new(configuration(true))?;
    ensure!(owner.workers.lock().tasks.len() == 2);
    let report = tokio::time::timeout(Duration::from_secs(5), owner.force_join()).await??;
    ensure!(report.quiescent && report.forced && !report.is_clean());
    ensure!(report.workers.joined == 2 && report.workers.cancelled == 2);
    let cached = owner.exporter.shutdown_recorded(true);
    ensure!(report.exporter == Some(cached));
    ensure!(!cached.is_clean());
    Ok(())
}

#[tokio::test]
async fn expired_deadline_does_not_start_exporter_shutdown() -> Result<()> {
    let owner = ProcessExporter::new(configuration(false))?;
    let expired = Instant::now() - Duration::from_millis(1);
    for _ in 0..2 {
        let report = owner.drain_until(expired).await?;
        ensure!(!report.started && !report.quiescent);
        ensure!(owner.lock().task.is_none());
        ensure!(!owner.workers.lock().closed);
    }
    ensure!(
        owner
            .drain_until(Instant::now() + Duration::from_secs(5))
            .await?
            .is_clean()
    );
    Ok(())
}

#[tokio::test]
async fn cancelled_waiter_retains_the_same_blocking_handle_and_worker_owner() -> Result<()> {
    let owner = ProcessExporter::new(configuration(false))?;
    let (release, wait) = std::sync::mpsc::channel();
    let (started, began) = tokio::sync::oneshot::channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let ran = calls.clone();
    let handle = tokio::task::spawn_blocking(move || {
        ran.fetch_add(1, Ordering::SeqCst);
        if started.send(()).is_err() {
            return ShutdownReport::default();
        }
        // Disconnection also releases the fixture if an earlier assertion fails.
        match wait.recv() {
            Ok(()) | Err(_) => ShutdownReport::default(),
        }
    });
    let task_id = handle.id();
    {
        let mut state = owner.lock();
        state.started = true;
        state.task = Some(handle);
    }
    tokio::time::timeout(Duration::from_secs(5), began).await??;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut first = Box::pin(owner.drain_until(deadline));
    ensure!(futures::poll!(first.as_mut()).is_pending());
    ensure!(owner.drain_until(deadline).await.is_err());
    drop(first);
    ensure!(owner.lock().task.as_ref().map(JoinHandle::id) == Some(task_id));
    let expired = owner
        .drain_until(Instant::now() - Duration::from_millis(1))
        .await?;
    ensure!(expired.started && !expired.quiescent && calls.load(Ordering::SeqCst) == 1);
    release.send(())?;
    let report = tokio::time::timeout(Duration::from_secs(5), owner.force_join()).await??;
    ensure!(report.quiescent && report.forced && calls.load(Ordering::SeqCst) == 1);
    Ok(())
}

struct DropCounter(Arc<AtomicUsize>);
impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn post_force_registration_is_joined_without_polling_its_work() -> Result<()> {
    let workers = Workers::new()?;
    workers.close(true);
    let polled = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = DropCounter(dropped.clone());
    let observed = polled.clone();
    workers.spawn(async move {
        let _guard = guard;
        observed.fetch_add(1, Ordering::SeqCst);
    });
    let report = std::future::poll_fn(|cx| workers.poll_joined(cx)).await;
    ensure!(report.joined == 1 && report.discarded == 1);
    ensure!(polled.load(Ordering::SeqCst) == 0 && dropped.load(Ordering::SeqCst) == 1);
    Ok(())
}

struct ReentrantPanic {
    workers: Arc<Workers>,
    dropped: Arc<AtomicUsize>,
    polled: Arc<AtomicUsize>,
}

impl Drop for ReentrantPanic {
    fn drop(&mut self) {
        let guard = DropCounter(self.dropped.clone());
        let polled = self.polled.clone();
        self.workers.spawn(async move {
            let _guard = guard;
            polled.fetch_add(1, Ordering::SeqCst);
        });
    }
}

#[tokio::test]
async fn panic_payload_destruction_reaches_the_join_fixed_point_outside_the_lock() -> Result<()> {
    let workers = Workers::new()?;
    let dropped = Arc::new(AtomicUsize::new(0));
    let polled = Arc::new(AtomicUsize::new(0));
    let payload = ReentrantPanic {
        workers: workers.clone(),
        dropped: dropped.clone(),
        polled: polled.clone(),
    };
    let (started, began) = tokio::sync::oneshot::channel();
    workers.spawn(async move {
        if started.send(()).is_ok() {
            std::panic::resume_unwind(Box::new(payload));
        }
    });
    began.await?;
    workers.close(false);
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        std::future::poll_fn(|cx| workers.poll_joined(cx)),
    )
    .await?;
    ensure!(report.panicked == 1 && report.discarded == 1 && report.joined == 2);
    ensure!(dropped.load(Ordering::SeqCst) == 1 && polled.load(Ordering::SeqCst) == 0);
    Ok(())
}

#[tokio::test]
async fn malformed_transport_configuration_starts_no_sdk_worker() -> Result<()> {
    let workers = Workers::new()?;
    let mut config = configuration(true);
    config.endpoint = "http://[invalid-private-endpoint".into();
    ensure!(OtelExporter::new_owned(config.clone(), workers.clone()).is_err());
    ensure!(workers.lock().tasks.is_empty());
    let error = ProcessExporter::new(config)
        .err()
        .context("invalid endpoint accepted")?;
    ensure!(error.to_string() == "process telemetry initialization failed");
    Ok(())
}

#[tokio::test]
async fn incomplete_owner_drop_keeps_provider_until_sdk_receivers_are_destroyed() -> Result<()> {
    for metrics in [false, true] {
        let owner = ProcessExporter::new(configuration(metrics))?;
        let exporter = Arc::downgrade(&owner.exporter);
        // No yield before Drop: processor cancellation has not run. Releasing
        // the last provider here used to block this single runtime thread in
        // the SDK's synchronous Drop shutdown.
        drop(owner);
        tokio::time::timeout(Duration::from_secs(5), async {
            while exporter.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn invalid_trace_capacity_starts_no_partial_provider() -> Result<()> {
    for metrics in [false, true] {
        for capacity in [0, tokio::sync::Semaphore::MAX_PERMITS + 1, usize::MAX] {
            let mut config = configuration(metrics);
            config.traces.batch.max_queue_size = capacity;
            let workers = Workers::new()?;
            ensure!(OtelExporter::new_owned(config.clone(), workers.clone()).is_err());
            ensure!(workers.lock().tasks.is_empty());
            let error = ProcessExporter::new(config.clone())
                .err()
                .context("invalid queue capacity accepted by process constructor")?;
            ensure!(error.to_string() == "process telemetry initialization failed");
            // The legacy constructor shares the preflight. It must return an
            // error rather than panic after starting an unowned metric reader.
            ensure!(OtelExporter::new(config, None).is_err());
        }
    }
    Ok(())
}

#[cfg(feature = "otel-http")]
#[tokio::test]
async fn cancelled_real_sdk_flush_resumes_the_original_shutdown() -> Result<()> {
    use opentelemetry::trace::{Span as _, Tracer as _};

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(AtomicUsize::new(0));
    let handler_entered = entered.clone();
    let handler_release = release.clone();
    let handler_requests = requests.clone();
    let router = axum::Router::new().route(
        "/v1/traces",
        axum::routing::post(move |body: axum::body::Bytes| {
            let entered = handler_entered.clone();
            let release = handler_release.clone();
            let requests = handler_requests.clone();
            async move {
                if body.is_empty() {
                    return http::StatusCode::BAD_REQUEST;
                }
                requests.fetch_add(1, Ordering::SeqCst);
                entered.notify_one();
                release.notified().await;
                http::StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::task::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                match stopped.await {
                    Ok(()) | Err(_) => {}
                }
            })
            .await
    });
    let result = async {
        let mut config = configuration(false);
        config.endpoint = format!("http://{address}/v1/traces");
        let owner = ProcessExporter::new(config)?;
        owner.exporter.tracer_clone().start("retained-flush").end();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut first = Box::pin(owner.drain_until(deadline));
        tokio::select! {
            report = &mut first => anyhow::bail!("shutdown completed before collector release: {report:?}"),
            arrived = tokio::time::timeout_at(deadline, entered.notified()) => arrived?,
        }
        let task_id = owner.lock().task.as_ref().map(JoinHandle::id);
        ensure!(task_id.is_some());
        drop(first);
        ensure!(owner.lock().task.as_ref().map(JoinHandle::id) == task_id);
        let mut resumed = Box::pin(owner.drain_until(deadline));
        ensure!(futures::poll!(resumed.as_mut()).is_pending());
        release.notify_one();
        let report = resumed.await?;
        ensure!(report.is_clean() && report.workers.joined == 1);
        ensure!(requests.load(Ordering::SeqCst) == 1);
        Ok::<(), anyhow::Error>(())
    }
    .await;
    // Release collector work even if the assertion path failed, and retain the
    // server handle through both normal and forced fixture teardown.
    release.notify_one();
    let stop_result = stop.send(());
    let joined = match tokio::time::timeout(Duration::from_secs(5), &mut server).await {
        Ok(joined) => joined,
        Err(_) => {
            server.abort();
            server.await
        }
    };
    result?;
    ensure!(stop_result.is_ok(), "collector stopped before shutdown");
    joined??;
    Ok(())
}
