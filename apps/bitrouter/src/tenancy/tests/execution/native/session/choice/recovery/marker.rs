//! A real MCP initialize socket must observe an ineligible durable marker first.
use super::*;
use crate::config::mcp::{McpServerConfig, McpTransport};
use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};

#[derive(Clone)]
struct Wire {
    received: Arc<Notify>,
    release: CancellationToken,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
async fn initialize(State(wire): State<Wire>, Json(request): Json<Value>) -> impl IntoResponse {
    if request["method"] != "initialize" || request.get("id").is_none() {
        return (http::StatusCode::BAD_REQUEST, Json(json!({})));
    }
    // https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle
    wire.calls.fetch_add(1, Ordering::AcqRel);
    wire.received.notify_one();
    wire.release.cancelled().await;
    (
        http::StatusCode::OK,
        Json(json!({"jsonrpc":"2.0","id":request["id"],"result":{
        "protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"held-native-marker","version":"1"}}})),
    )
}
pub(super) fn config(url: &str) -> HarnessConfig {
    HarnessConfig {
        servers: vec![McpServerConfig::with_defaults(
            "marker-peer",
            McpTransport::Http {
                url: url.into(),
                headers: Default::default(),
            },
        )],
        ..Default::default()
    }
}
async fn payload(db: &sea_orm::DatabaseConnection) -> Result<String> {
    Ok(db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT payload FROM bro_managed_sessions",
        ))
        .await?
        .context("native opening marker")?
        .try_get("", "payload")?)
}
async fn campaign(root: &Path, mode: &str) -> Result<()> {
    let generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::start(false, false).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    let wire = Wire {
        received: Arc::new(Notify::new()),
        release: CancellationToken::new(),
        calls: Default::default(),
    };
    let stop = CancellationToken::new();
    let done = stop.clone();
    let router = Router::new()
        .route("/mcp", post(initialize))
        .with_state(wire.clone());
    let server = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(done.cancelled_owned())
            .await
    }));
    std::fs::create_dir(root.join("workspace"))?;
    let initial = fixture::sqlite(&generation, &choice, root, 2).await?;
    let mut spec = Spec::capture(&initial, &generation)?;
    spec.mcp = Some(url);
    private_json(&root.join("spec.json"), &spec)?;
    initial.fixture.service.shutdown().await?;
    initial.fixture.db.clone().close().await?;
    drop(initial);
    tokio::time::timeout(TIMEOUT, async {
        loop {
            match crate::tenancy::ownership::acquire_path(&root.join("owned.db"), false) {
                Ok(lease) => {
                    drop(lease);
                    break Ok::<_, anyhow::Error>(());
                }
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    tokio::task::yield_now().await
                }
                Err(error) => break Err(error),
            }
        }
    })
    .await??;
    let mut child = Child::start("native_stopped_owner_producer", root, mode)?;
    let observed = async {
        child.line("native-marker-opening").await?;
        tokio::time::timeout(TIMEOUT, wire.received.notified()).await?;
        let reader = crate::db::connect(&format!(
            "sqlite://{}?mode=ro",
            root.join("owned.db").display()
        ))
        .await?;
        let bytes = payload(&reader).await?;
        reader.close().await?;
        let value: Value = serde_json::from_str(&bytes)?;
        ensure!(
            value["format"] == 2
                && value["recovery"]["phase"] == "opening"
                && value["recovery"]["eligible"] == false
                && value["journal"]["opening"] == true
                && value["journal"].get("manifest").is_none()
        );
        if mode == "marker-released" {
            let prior: Value =
                serde_json::from_slice(&std::fs::read(root.join("released-before.json"))?)?;
            ensure!(prior["released"] == true && value["journal"]["previous"] == prior);
        } else {
            ensure!(value["journal"]["previous"].is_null());
        }
        ensure!(
            generation.calls() == 0
                && choice.calls() == 0
                && wire.calls.load(Ordering::Acquire) == 1
        );
        Ok::<_, anyhow::Error>(bytes)
    }
    .await;
    let reaped = child.reap(true).await;
    let checked = async {
        let before = observed?;
        reaped?;
        let bound = spec.connect(root).await?;
        // Reopen with the same mode so the durable ineligible marker, rather
        // than an unrelated read-only mismatch, must reject the predecessor.
        let app = Arc::new(
            host_app(
                &bound.fixture,
                &generation.url,
                Arc::new(HostSpies::default()),
            )?
            .with_tenancy(bound.fixture.service.clone()),
        );
        let rejected = NativeSession::open_authenticated(
            app,
            bound.headers.clone(),
            "choice-session",
            &root.join("workspace"),
            ToolMode::Coding,
            &HarnessConfig::default(),
        )
        .await;
        ensure!(rejected.is_err(), "ineligible discovery marker reopened");
        ensure!(
            payload(&bound.fixture.db).await? == before,
            "refusal rewrote marker/previous bytes"
        );
        tokio::time::timeout(TIMEOUT, async {
            while bound.fixture.service.host_slots_for_test() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        bound.fixture.service.shutdown().await?;
        // Host-slot return does not quiesce H maintenance; the same Service drain does.
        bound.fixture.service.cleanup.ensure_clean()?;
        ensure!(
            generation.calls() == 0
                && choice.calls() == 0
                && wire.calls.load(Ordering::Acquire) == 1
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    wire.release.cancel();
    stop.cancel();
    let joined = tokio::time::timeout(TIMEOUT, server).await;
    let generation_closed = generation.finish().await;
    let choice_closed = choice.finish().await;
    checked?;
    joined???;
    generation_closed?;
    choice_closed
}

#[tokio::test]
#[ignore = "isolated real pre-discovery marker and producer death campaign"]
async fn native_stopped_owner_marker_precedes_discovery_and_preserves_released_source() -> Result<()>
{
    for mode in ["marker-new", "marker-released"] {
        let root = tempfile::tempdir()?;
        // Controller shares this process's explicit key only through child env.
        let mut child = Child::start("marker::native_marker_controller", root.path(), mode)?;
        let observed = child.line("native-marker-checked").await;
        let reaped = child.reap(observed.is_err()).await;
        observed?;
        reaped?;
    }
    Ok(())
}
#[tokio::test]
#[ignore = "internal pre-discovery controller; select the parent only"]
async fn native_marker_controller() -> Result<()> {
    let Some(root) = std::env::var_os(ROOT) else {
        return Ok(());
    };
    campaign(Path::new(&root), &std::env::var(MODE)?).await?;
    signal("native-marker-checked")
}
