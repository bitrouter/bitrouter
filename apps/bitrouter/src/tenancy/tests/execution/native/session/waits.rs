//! Later real MCP waits must not turn an earlier authority check into permission.
use super::*;
use crate::{
    config::mcp::{McpServerConfig, McpTransport},
    tenancy::hosts::{HostBoundary, HostGate},
};

mod approval;
mod pagination;

#[derive(Clone)]
struct Wire {
    paginated: bool,
    pages: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    release: CancellationToken,
    stop: CancellationToken,
}

async fn rpc(State(wire): State<Wire>, Json(request): Json<Value>) -> Response {
    let Some(id) = request.get("id") else {
        return StatusCode::ACCEPTED.into_response();
    };
    // Exercise real stateless MCP initialization, pagination and tool results.
    // https://modelcontextprotocol.io/specification/2025-11-25/server/tools
    // https://modelcontextprotocol.io/specification/2025-11-25/server/utilities/pagination
    let result = match request["method"].as_str() {
        Some("initialize") => json!({"protocolVersion":"2025-11-25",
            "capabilities":{"tools":{}},"serverInfo":{"name":"authority-wait","version":"1"}}),
        Some("tools/list") => {
            let page = wire.pages.fetch_add(1, Ordering::SeqCst) + 1;
            if wire.paginated && page == 2 {
                if request["params"]["cursor"] != "second" {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                wire.entered.notify_one();
                tokio::select! {
                    () = wire.release.cancelled() => {},
                    () = wire.stop.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
                }
            }
            let name = if page == 1 {
                "echo".to_owned()
            } else {
                format!("echo{page}")
            };
            let mut reply = json!({"tools":[{"name":name,"inputSchema":{"type":"object"}}]});
            if wire.paginated && page < 3 {
                reply["nextCursor"] = json!(if page == 1 { "second" } else { "third" });
            }
            if wire.paginated && page == 3 && request["params"]["cursor"] != "third" {
                return StatusCode::BAD_REQUEST.into_response();
            }
            reply
        }
        Some("tools/call") => {
            wire.calls.fetch_add(1, Ordering::SeqCst);
            if request["params"]["name"] != "echo" {
                return StatusCode::BAD_REQUEST.into_response();
            }
            json!({"content":[{"type":"text","text":"real MCP effect completed"}],"isError":false})
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
    async fn start(paginated: bool) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let wire = Wire {
            paginated,
            pages: Arc::new(AtomicUsize::new(0)),
            calls: Arc::new(AtomicUsize::new(0)),
            entered: Arc::new(Notify::new()),
            release: CancellationToken::new(),
            stop: CancellationToken::new(),
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
    fn config(&self) -> HarnessConfig {
        HarnessConfig {
            servers: vec![McpServerConfig::with_defaults(
                "retained",
                McpTransport::Http {
                    url: self.url.clone(),
                    headers: Default::default(),
                },
            )],
            ..HarnessConfig::default()
        }
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

type McpWitness = std::sync::Weak<Mutex<crate::harness::resources::mcp::McpConnections>>;

async fn observe(gate: &HostGate) -> Result<McpWitness> {
    let observed = async {
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        let owner = gate.mcp.get().context("actual opening owner")?.clone();
        ensure!(owner.upgrade().is_some(), "MCP owner disappeared");
        ensure!(
            !gate
                .http
                .get()
                .context("actual HTTP worker")?
                .joined()
                .map_err(anyhow::Error::msg)?
        );
        Ok(owner)
    }
    .await;
    gate.release();
    observed
}

fn joined(gate: &HostGate) -> Result<bool> {
    gate.http
        .get()
        .context("actual HTTP worker")?
        .joined()
        .map_err(anyhow::Error::msg)
}

async fn saved(fixture: &Fixture) -> Result<Value> {
    let row = fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT payload FROM bro_managed_sessions",
        ))
        .await?
        .context("native journal")?;
    Ok(serde_json::from_str(
        &row.try_get::<String>("", "payload")?,
    )?)
}
