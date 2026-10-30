//! A real HTTP initialize request holds rmcp's actual worker before handshake.
use super::*;

#[derive(Clone)]
struct Wire {
    entered: Arc<Notify>,
    release: CancellationToken,
    stop: CancellationToken,
    requests: Arc<AtomicUsize>,
}

async fn initialize(State(wire): State<Wire>, Json(request): Json<Value>) -> Response {
    wire.requests.fetch_add(1, Ordering::SeqCst);
    if request["method"] != "initialize" || request.get("id").is_none() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    wire.entered.notify_one();
    tokio::select! {
        () = wire.release.cancelled() => {},
        () = wire.stop.cancelled() => {},
    }
    // A real terminal HTTP error ends the held initialization operation. It
    // does not fabricate a successful handshake or enqueue a cleanup request.
    StatusCode::SERVICE_UNAVAILABLE.into_response()
}

struct Endpoint {
    url: String,
    wire: Wire,
    task: Option<AbortOnDropHandle<std::io::Result<()>>>,
}
impl Endpoint {
    async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let wire = Wire {
            entered: Arc::new(Notify::new()),
            release: CancellationToken::new(),
            stop: CancellationToken::new(),
            requests: Arc::new(AtomicUsize::new(0)),
        };
        let router = Router::new()
            .route("/mcp", post(initialize))
            .with_state(wire.clone());
        let stop = wire.stop.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
        }));
        Ok(Self {
            url,
            wire,
            task: Some(task),
        })
    }

    async fn finish(&mut self) -> Result<()> {
        self.wire.release.cancel();
        self.wire.stop.cancel();
        if let Some(task) = self.task.take() {
            tokio::time::timeout(TIMEOUT, task).await???;
        }
        Ok(())
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.wire.release.cancel();
        self.wire.stop.cancel();
    }
}

pub(super) async fn exercise(root: &Path, mode: &str) -> Result<()> {
    ensure!(matches!(
        mode,
        "http-cancel" | "http-timeout" | "http-confirmed"
    ));
    let provider = Provider::start(false).await?;
    let mut endpoint = Endpoint::start().await?;
    // A listening second endpoint detects even a connect with no HTTP payload.
    let second = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, root).await?;
    let (headers, _) = ready(&fixture, &route).await?;
    let app = Arc::new(
        host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone()),
    );
    let gate = fixture
        .service
        .hold_host(crate::tenancy::hosts::HostBoundary::Transport)?;
    let config = HarnessConfig {
        servers: vec![
            McpServerConfig::with_defaults(
                "first",
                McpTransport::Http {
                    url: endpoint.url.clone(),
                    headers: Default::default(),
                },
            ),
            McpServerConfig::with_defaults(
                "second",
                McpTransport::Http {
                    url: format!("http://{}/mcp", second.local_addr()?),
                    headers: Default::default(),
                },
            ),
        ],
        ..HarnessConfig::default()
    };
    let mut opener = Some(AbortOnDropHandle::new(tokio::spawn(async move {
        NativeSession::open_authenticated(
            app,
            headers,
            "http-startup",
            &workspace,
            ToolMode::Coding,
            &config,
        )
        .await
    })));
    tokio::time::timeout(TIMEOUT, endpoint.wire.entered.notified()).await?;
    let weak = gate.mcp.get().context("actual opening MCP owner")?.clone();
    let worker = gate.http.get().context("actual HTTP worker observer")?;
    ensure!(!worker.joined().map_err(anyhow::Error::msg)?);
    ensure!(fixture.service.host_slots_for_test() == 0);
    // Retain a test observer only in the clean case, so its acknowledged state
    // remains inspectable after production releases the owner. The unknown
    // cases keep only weak observers until production quarantine is proven.
    let confirmed = if mode == "http-confirmed" {
        Some(weak.upgrade().context("opening owner missing")?)
    } else {
        None
    };
    if mode == "http-timeout" {
        // The initialize request has reached the peer, proving CONNECT_TIMEOUT
        // is already armed. Advancing only controls time, never ordering.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::time::resume();
    } else {
        let waiter = opener.take().context("opener waiter")?;
        waiter.abort();
        ensure!(matches!(waiter.await, Err(error) if error.is_cancelled()));
    }
    tokio::time::timeout(TIMEOUT, worker.closing())
        .await?
        .map_err(anyhow::Error::msg)?;
    ensure!(!worker.joined().map_err(anyhow::Error::msg)?);
    if mode == "http-confirmed" {
        endpoint.wire.release.cancel();
        tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await??;
        ensure!(worker.joined().map_err(anyhow::Error::msg)?);
        fixture.service.cleanup.ensure_clean()?;
        ensure!(fixture.service.host_slots_for_test() == 1);
        drop(confirmed);
        ensure!(weak.upgrade().is_none(), "clean opener retained MCP owner");
    } else {
        // The SAME close future is now being polled. The endpoint remains
        // blocked beyond cleanup's own deadline, independently of the opener.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::time::resume();
        tokio::time::timeout(TIMEOUT, fixture.service.cleanup.quarantined()).await?;
        if let Some(waiter) = opener.take() {
            ensure!(tokio::time::timeout(TIMEOUT, waiter).await??.is_err());
        }
        ensure!(fixture.service.current().is_err());
        ensure!(fixture.service.host_slots_for_test() == 0);
        ensure!(!worker.joined().map_err(anyhow::Error::msg)?);
        no_clean_drain(&fixture).await?;
        let retained = weak
            .upgrade()
            .context("quarantine lost the actual HTTP owner")?;
        // Return the terminal HTTP error, then drive the SAME stored worker
        // join through the production MCP shutdown path. No replacement task,
        // transport, handshake, cleanup receipt or Service retry is created.
        endpoint.wire.release.cancel();
        tokio::time::timeout(TIMEOUT, async { retained.lock().await.shutdown().await })
            .await?
            .map_err(anyhow::Error::msg)?;
        ensure!(worker.joined().map_err(anyhow::Error::msg)?);
        drop(retained);
        ensure!(weak.upgrade().is_some(), "test became the sole owner");
        ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous);
        ensure!(fixture.service.host_slots_for_test() == 0);
        ensure!(fixture.service.completion.clone().now_or_never().is_none());
    }
    ensure!(endpoint.wire.requests.load(Ordering::SeqCst) == 1);
    ensure!(
        matches!(second.into_std()?.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "later MCP socket started"
    );
    ensure!(provider.calls() == 0);
    fixture.db.clone().close().await?;
    endpoint.finish().await?;
    provider.finish().await?;
    // Parent probes the file lock after all ordinary Service/App/DB handles
    // have dropped. Unknown cases must still own it even after test-driven join.
    Ok(())
}

#[tokio::test]
async fn native36_http_initialize_worker_and_file_owner_survive_cancel_and_timeout() -> Result<()> {
    for mode in ["http-confirmed", "http-cancel", "http-timeout"] {
        isolated(mode).await?;
    }
    Ok(())
}
