//! Hold actual MCP catalog I/O, then observe the next connection admission.
use super::*;
use crate::config::mcp::{McpServerConfig, McpTransport};

#[derive(Clone)]
struct Wire {
    catalog: Arc<Notify>,
    release: CancellationToken,
    stop: CancellationToken,
    requests: Arc<AtomicUsize>,
}

async fn rpc(State(wire): State<Wire>, Json(request): Json<Value>) -> Response {
    wire.requests.fetch_add(1, Ordering::SeqCst);
    let Some(id) = request.get("id") else {
        return StatusCode::ACCEPTED.into_response();
    };
    // Real MCP initialize and tools/list replies; no provider-wire projection.
    // https://modelcontextprotocol.io/specification/2025-11-25/server/tools
    let result = match request["method"].as_str() {
        Some("initialize") => {
            json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"held-catalog","version":"1"}})
        }
        Some("tools/list") => {
            wire.catalog.notify_one();
            tokio::select! {
                () = wire.release.cancelled() => {},
                () = wire.stop.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            }
            json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]})
        }
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
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
            catalog: Arc::new(Notify::new()),
            release: CancellationToken::new(),
            stop: CancellationToken::new(),
            requests: Arc::new(AtomicUsize::new(0)),
        };
        let router = Router::new()
            .route("/mcp", post(rpc))
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

// A connection detector, deliberately not an MCP implementation. No permitted
// native operation may reach it after the held first endpoint loses authority.
struct SocketTrap {
    url: String,
    accepts: Arc<AtomicUsize>,
    stop: CancellationToken,
    task: Option<AbortOnDropHandle<std::io::Result<()>>>,
}
impl SocketTrap {
    async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let accepts = Arc::new(AtomicUsize::new(0));
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let count = accepts.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = stopped.cancelled() => return Ok(()),
                    accepted = listener.accept() => {
                        let (socket, _) = accepted?;
                        count.fetch_add(1, Ordering::SeqCst);
                        drop(socket);
                    }
                }
            }
        }));
        Ok(Self {
            url,
            accepts,
            stop,
            task: Some(task),
        })
    }
    async fn finish(&mut self) -> Result<()> {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            tokio::time::timeout(TIMEOUT, task).await???;
        }
        Ok(())
    }
}
impl Drop for SocketTrap {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[tokio::test]
async fn native_catalog_wait_rechecks_before_the_next_socket_admission() -> Result<()> {
    for change in ["allowed", "revoke", "scope", "cancel", "shutdown"] {
        let provider = Provider::start(false).await?;
        let mut first = Endpoint::start().await?;
        let mut second = SocketTrap::start().await?;
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let (fixture, route) = file_fixture(&provider, directory.path()).await?;
        let checked = tokio::time::timeout(Duration::from_secs(20), async {
            let (headers, key) = ready(&fixture, &route).await?;
            let app = Arc::new(
                host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                    .with_tenancy(fixture.service.clone()),
            );
            let config = HarnessConfig {
                servers: vec![
                    McpServerConfig::with_defaults(
                        "first",
                        McpTransport::Http {
                            url: first.url.clone(),
                            headers: Default::default(),
                        },
                    ),
                    McpServerConfig::with_defaults(
                        "second",
                        McpTransport::Http {
                            url: second.url.clone(),
                            headers: Default::default(),
                        },
                    ),
                ],
                ..HarnessConfig::default()
            };
            let opener = AbortOnDropHandle::new(tokio::spawn(async move {
                NativeSession::open_authenticated(
                    app,
                    headers,
                    "catalog-wait",
                    &workspace,
                    ToolMode::Coding,
                    &config,
                )
                .await
            }));
            first.wire.catalog.notified().await;
            ensure!(
                fixture.service.host_slots_for_test() == 0
                    && second.accepts.load(Ordering::SeqCst) == 0
            );
            if matches!(change, "revoke" | "scope") {
                change_authority(&fixture, &key, &route, change).await?;
            }
            if change == "cancel" {
                opener.abort();
                ensure!(matches!(opener.await, Err(error) if error.is_cancelled()));
                first.wire.release.cancel();
                fixture.service.shutdown().await?;
            } else if change == "shutdown" {
                let service = fixture.service.clone();
                let shutdown =
                    AbortOnDropHandle::new(tokio::spawn(async move { service.shutdown().await }));
                fixture.service.host_stop().cancelled().await;
                first.wire.release.cancel();
                ensure!(opener.await?.is_err());
                shutdown.await??;
            } else {
                first.wire.release.cancel();
                // The allowed calibration reaches the detector and gets EOF;
                // all other cases must fail without even connecting to it.
                ensure!(opener.await?.is_err());
                fixture.service.shutdown().await?;
            }
            if change == "allowed" {
                ensure!(
                    second.accepts.load(Ordering::SeqCst) > 0,
                    "connection probe was never exercised"
                );
            } else {
                ensure!(
                    second.accepts.load(Ordering::SeqCst) == 0,
                    "{change}: a later MCP connection escaped its authority gate"
                );
            }
            ensure!(first.wire.requests.load(Ordering::SeqCst) >= 2);
            ensure!(
                fixture.service.host_slots_for_test() == 1,
                "first MCP owner did not drain"
            );
            ensure!(
                fixture.service.cleanup.disposition()
                    != crate::tenancy::transaction::Disposition::Ambiguous
            );
            ensure!(provider.calls() == 0);
            Ok(())
        })
        .await
        .context("native discovery matrix deadline")
        .and_then(|outcome| outcome);
        first.wire.release.cancel();
        let first_closed = first.finish().await;
        let second_closed = second.finish().await;
        let cleaned = finish(&fixture, provider, checked).await;
        first_closed?;
        second_closed?;
        cleaned?;
    }
    Ok(())
}
