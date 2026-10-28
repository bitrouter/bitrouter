//! Hold actual MCP catalog I/O, then observe the next connection admission.
use super::*;
use crate::config::mcp::{McpServerConfig, McpTransport};
use crate::harness::resources::managed::store::NativeStore;

struct ObservedStore {
    inner: crate::managed_store::DatabaseNativeStore,
    loads: AtomicUsize,
    saves: AtomicUsize,
}

#[async_trait]
impl NativeStore for ObservedStore {
    async fn load(&self) -> std::result::Result<Option<(u64, Vec<u8>)>, CoreError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load().await
    }

    async fn save(&self, revision: u64, bytes: Vec<u8>) -> std::result::Result<(), CoreError> {
        self.saves.fetch_add(1, Ordering::SeqCst);
        self.inner.save(revision, bytes).await
    }
}

#[derive(Clone)]
struct Wire {
    catalog: Arc<Notify>,
    held_catalog: &'static str,
    release: CancellationToken,
    stop: CancellationToken,
    requests: Arc<AtomicUsize>,
    tool_requests: Arc<AtomicUsize>,
    skill_requests: Arc<AtomicUsize>,
}

async fn rpc(State(wire): State<Wire>, Json(request): Json<Value>) -> Response {
    wire.requests.fetch_add(1, Ordering::SeqCst);
    let Some(id) = request.get("id") else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = request["method"].as_str();
    if method == Some("tools/list") {
        wire.tool_requests.fetch_add(1, Ordering::SeqCst);
    } else if method == Some("skills/list") {
        wire.skill_requests.fetch_add(1, Ordering::SeqCst);
    }
    if method == Some(wire.held_catalog) {
        wire.catalog.notify_one();
        tokio::select! {
            () = wire.release.cancelled() => {},
            () = wire.stop.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    }
    // Real MCP initialize and catalog replies; no provider-wire projection.
    // https://modelcontextprotocol.io/specification/2025-11-25/server/tools
    let result = match method {
        Some("initialize") => {
            let capabilities = if wire.held_catalog == "skills/list" {
                json!({"tools":{},"resources":{},"extensions":{"io.modelcontextprotocol/skills":{}}})
            } else {
                json!({"tools":{}})
            };
            json!({"protocolVersion":"2025-11-25","capabilities":capabilities,"serverInfo":{"name":"held-catalog","version":"1"}})
        }
        Some("tools/list") => {
            json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]})
        }
        Some("skills/list") => {
            let mut page =
                json!({"skills":[],"resultType":"complete","ttlMs":0,"cacheScope":"private"});
            match request["params"].get("cursor") {
                None => page["nextCursor"] = json!("page-2"),
                Some(Value::String(cursor)) if cursor == "page-2" => {}
                _ => return StatusCode::BAD_REQUEST.into_response(),
            }
            page
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
        Self::start_catalog("tools/list").await
    }

    async fn start_catalog(held_catalog: &'static str) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let wire = Wire {
            catalog: Arc::new(Notify::new()),
            held_catalog,
            release: CancellationToken::new(),
            stop: CancellationToken::new(),
            requests: Arc::new(AtomicUsize::new(0)),
            tool_requests: Arc::new(AtomicUsize::new(0)),
            skill_requests: Arc::new(AtomicUsize::new(0)),
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
async fn legacy_openers_reject_tenancy_before_store_workspace_or_mcp_io() -> Result<()> {
    let provider = Provider::start(false).await?;
    let mut trap = SocketTrap::start().await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    let missing = directory.path().join("does-not-exist");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, directory.path()).await?;
    let checked = tokio::time::timeout(TIMEOUT, async {
        ready(&fixture, &route).await?;
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let store = Arc::new(ObservedStore {
            inner: crate::managed_store::DatabaseNativeStore::new(
                fixture.db.clone(),
                "legacy-probe".into(),
            )?,
            loads: AtomicUsize::new(0),
            saves: AtomicUsize::new(0),
        });
        let config = HarnessConfig {
            servers: vec![McpServerConfig::with_defaults(
                "probe",
                McpTransport::Http {
                    url: trap.url.clone(),
                    headers: Default::default(),
                },
            )],
            ..HarnessConfig::default()
        };
        for path in [&missing, &workspace] {
            for cancellable in [false, true] {
                let caller = CallerContext::new("forged", "principal-a");
                let opened = if cancellable {
                    NativeSession::open_cancellable(
                        app.clone(),
                        caller,
                        store.clone(),
                        path,
                        ToolMode::Coding,
                        &config,
                        &CancellationToken::new(),
                    )
                    .await
                } else {
                    NativeSession::open(
                        app.clone(),
                        caller,
                        store.clone(),
                        path,
                        ToolMode::Coding,
                        &config,
                    )
                    .await
                };
                let denied = opened.err().context("legacy opener accepted tenancy")?;
                ensure!(
                    denied.code == bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope,
                    "cancellable={cancellable}, path={}: workspace or resource work preceded rejection",
                    path.display()
                );
                ensure!(
                    denied.commit_status
                        == bitrouter_ai::core::protocol::CommitStatus::NotCommitted
                );
                ensure!(store.loads.load(Ordering::SeqCst) == 0);
                ensure!(store.saves.load(Ordering::SeqCst) == 0);
                ensure!(trap.accepts.load(Ordering::SeqCst) == 0);
            }
        }
        ensure!(!missing.exists());
        ensure!(std::fs::read_dir(&workspace)?.next().is_none());
        let row = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS total FROM bro_managed_sessions",
            ))
            .await?
            .context("native count")?;
        ensure!(row.try_get::<i64>("", "total")? == 0);
        ensure!(fixture.service.host_slots_for_test() == 1 && provider.calls() == 0);

        // Calibrate the very same real store and socket detector using an App
        // without tenancy: discovery must reach the listener and fail on EOF.
        let legacy = Arc::new(host_app(
            &fixture,
            &provider.url,
            Arc::new(HostSpies::default()),
        )?);
        let attempted = NativeSession::open_cancellable(
            legacy,
            CallerContext::new("forged", "principal-a"),
            store.clone(),
            &workspace,
            ToolMode::Coding,
            &config,
            &CancellationToken::new(),
        )
        .await;
        ensure!(attempted.is_err(), "socket detector unexpectedly served MCP");
        ensure!(
            store.loads.load(Ordering::SeqCst) > 0,
            "store detector was not exercised"
        );
        ensure!(
            trap.accepts.load(Ordering::SeqCst) > 0,
            "socket detector was not exercised"
        );
        ensure!(store.saves.load(Ordering::SeqCst) == 0 && provider.calls() == 0);
        Ok(())
    })
    .await
    .context("legacy opener I/O deadline")
    .and_then(|outcome| outcome);
    let trap_closed = trap.finish().await;
    let cleaned = finish(&fixture, provider, checked).await;
    trap_closed?;
    cleaned
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

#[tokio::test]
async fn native_skill_catalog_revocation_blocks_next_page_and_next_source() -> Result<()> {
    for change in ["allowed", "revoke"] {
        let provider = Provider::start(false).await?;
        let mut first = Endpoint::start_catalog("skills/list").await?;
        let mut second = Endpoint::start_catalog("skills/list").await?;
        second.wire.release.cancel();
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
            let mut config = HarnessConfig::default();
            for (name, endpoint) in [("first", &first), ("second", &second)] {
                config.servers.push(McpServerConfig::with_defaults(
                    name,
                    McpTransport::Http {
                        url: endpoint.url.clone(),
                        headers: Default::default(),
                    },
                ));
                config.skills.mcp_sources.insert(
                    name.into(),
                    crate::config::skills::McpSkillSourceGrant {
                        workspaces: vec![workspace.clone()],
                        api_key_ids: vec![key.credential_id.to_string()],
                        ..Default::default()
                    },
                );
            }
            let opener = AbortOnDropHandle::new(tokio::spawn(async move {
                NativeSession::open_authenticated(
                    app,
                    headers,
                    "skill-catalog-wait",
                    &workspace,
                    ToolMode::ReadOnly,
                    &config,
                )
                .await
            }));
            first.wire.catalog.notified().await;
            ensure!(fixture.service.host_slots_for_test() == 0);
            ensure!(first.wire.skill_requests.load(Ordering::SeqCst) == 1);
            ensure!(second.wire.skill_requests.load(Ordering::SeqCst) == 0);
            if change == "revoke" {
                change_authority(&fixture, &key, &route, change).await?;
            }
            first.wire.release.cancel();
            let opened = opener.await?;
            if change == "allowed" {
                let session = opened?;
                // Calibration proves the scoped caller reaches both real
                // paginated catalogs even with all MCP tools disabled.
                ensure!(first.wire.skill_requests.load(Ordering::SeqCst) == 2);
                ensure!(second.wire.skill_requests.load(Ordering::SeqCst) == 2);
                session.close().await?;
            } else {
                ensure!(
                    opened.is_err(),
                    "revoked skill discovery delivered a session"
                );
                fixture.service.shutdown().await?;
                ensure!(
                    first.wire.skill_requests.load(Ordering::SeqCst) == 1,
                    "revoked catalog fetched another page"
                );
                ensure!(
                    second.wire.skill_requests.load(Ordering::SeqCst) == 0,
                    "revoked catalog continued to another source"
                );
            }
            ensure!(first.wire.tool_requests.load(Ordering::SeqCst) == 0);
            ensure!(second.wire.tool_requests.load(Ordering::SeqCst) == 0);
            ensure!(fixture.service.host_slots_for_test() == 1);
            ensure!(
                fixture.service.cleanup.disposition()
                    != crate::tenancy::transaction::Disposition::Ambiguous
            );
            let row = fixture
                .db
                .query_one(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "SELECT COUNT(*) AS total FROM bro_managed_sessions",
                ))
                .await?
                .context("native count")?;
            ensure!(row.try_get::<i64>("", "total")? == i64::from(change == "allowed"));
            ensure!(provider.calls() == 0);
            Ok(())
        })
        .await
        .context("native skill discovery deadline")
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
