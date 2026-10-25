//! Real provider and gateway sockets shared only by this finite test batch.
use super::*;
use crate::tenancy::{Service, resources::Resources};
use std::path::Path;

#[derive(Clone, Copy)]
pub(super) enum Protocol {
    Chat,
    Responses,
}
impl Protocol {
    fn name(self) -> &'static str {
        match self {
            Self::Chat => "chat_completions",
            Self::Responses => "responses",
        }
    }
    fn path(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
        }
    }
}
#[derive(Clone)]
struct Endpoint {
    protocol: Protocol,
    response: Value,
    usage: Value,
    open: CancellationToken,
    tail: CancellationToken,
    calls: Arc<AtomicUsize>,
    observed: mpsc::Sender<Value>,
}
async fn respond(
    State(state): State<Endpoint>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> Response {
    if !headers
        .get("authorization")
        .is_some_and(|v| v == "Bearer owned-upstream-secret")
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state.calls.fetch_add(1, Ordering::SeqCst);
    let streaming = input.get("stream") == Some(&Value::Bool(true));
    if state.observed.try_send(input).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    state.open.cancelled().await;
    if !streaming {
        return Json(state.response).into_response();
    }
    let (prefix, terminal) = match state.protocol {
        Protocol::Chat => (
            format!(
                "data: {}\n\n",
                json!({"id":"chat-fault","object":"chat.completion.chunk","model":"model-a","choices":[{"index":0,"delta":{"role":"assistant","content":"owned reply"},"finish_reason":null}]})
            ),
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"id":"chat-fault","object":"chat.completion.chunk","model":"model-a","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":state.usage})
            ),
        ),
        Protocol::Responses => (
            format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"type":"response.created","response":{"id":"resp-fault-private","status":"in_progress"}}),
                json!({"type":"response.output_text.delta","delta":"owned reply"})
            ),
            format!(
                "data: {}\n\n",
                json!({"type":"response.completed","response":state.response})
            ),
        ),
    };
    let body = async_stream::stream! {
        yield Ok::<_, std::io::Error>(Bytes::from(prefix));
        state.tail.cancelled().await;
        yield Ok(Bytes::from(terminal));
    };
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(body),
    )
        .into_response()
}
pub(super) struct Listener {
    pub(super) base: String,
    stop: CancellationToken,
    task: AbortOnDropHandle<std::io::Result<()>>,
}
impl Listener {
    async fn start(app: Router) -> Result<Self> {
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", socket.local_addr()?);
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(socket, app)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
        }));
        Ok(Self { base, stop, task })
    }
    pub(super) async fn close(self) -> Result<()> {
        self.stop.cancel();
        tokio::time::timeout(WAIT, self.task).await???;
        Ok(())
    }
}
pub(super) struct Provider {
    listener: Listener,
    pub(super) open: CancellationToken,
    pub(super) tail: CancellationToken,
    pub(super) calls: Arc<AtomicUsize>,
    observed: mpsc::Receiver<Value>,
    _release_open: tokio_util::sync::DropGuard,
    _release_tail: tokio_util::sync::DropGuard,
}
impl Provider {
    async fn start(protocol: Protocol) -> Result<Self> {
        let usage = super::super::full_usage(protocol.name());
        let response = match protocol {
            Protocol::Chat => super::super::json_response(protocol.name(), usage.clone())?,
            Protocol::Responses => {
                json!({"id":"resp-fault-private","object":"response","status":"completed","store":true,"model":"model-a","output":[{"id":"msg-fault-private","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"owned reply","annotations":[]}]}],"usage":usage})
            }
        };
        let open = CancellationToken::new();
        let tail = CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let (sender, observed) = mpsc::channel(8);
        let app = Router::new()
            .route(protocol.path(), post(respond))
            .with_state(Endpoint {
                protocol,
                response,
                usage,
                open: open.clone(),
                tail: tail.clone(),
                calls: calls.clone(),
                observed: sender,
            });
        Ok(Self {
            listener: Listener::start(app).await?,
            _release_open: open.clone().drop_guard(),
            _release_tail: tail.clone().drop_guard(),
            open,
            tail,
            calls,
            observed,
        })
    }
    pub(super) async fn accepted(&mut self) -> Result<Value> {
        tokio::time::timeout(WAIT, self.observed.recv())
            .await?
            .context("authenticated provider POST")
    }
    pub(super) async fn close(self) -> Result<()> {
        self.open.cancel();
        self.tail.cancel();
        self.listener.close().await
    }
}
pub(super) struct Harness {
    pub(super) fixture: Fixture,
    pub(super) provider: Provider,
    pub(super) ingress: Listener,
    pub(super) headers: HeaderMap,
    protocol: Protocol,
}
impl Harness {
    pub(super) async fn new(protocol: Protocol, path: Option<&Path>) -> Result<Self> {
        let provider = Provider::start(protocol).await?;
        let url = format!("{}/v1", provider.listener.base);
        let owner = path
            .map(|path| crate::tenancy::ownership::acquire_path(path, true))
            .transpose()?;
        let initial = if let Some(owner) = &owner {
            let db =
                crate::db::connect_tenancy(&format!("sqlite://{}", owner.path.display()), owner)
                    .await?;
            Fixture::with_database(db, Default::default(), &mut |_| {}).await?
        } else {
            Fixture::new().await?
        };
        let (mut fixture, route, _) = super::super::super::provision_protocol_with_fixture(
            initial,
            &url,
            protocol.name(),
            &mut |_| {},
        )
        .await?;
        if let Some(owner) = owner {
            fixture.service.shutdown().await?;
            let snapshot = crate::tenancy::store::load(&fixture.db, chrono::Utc::now()).await?;
            fixture.service = Service::start_prepared(
                fixture.db.clone(),
                Some(owner),
                Default::default(),
                snapshot,
                Arc::new(Resources::with_key(fixture.installation.clone(), [29; 32])?),
            )
            .await?;
        }
        let revision = draft(&fixture, &route, "receipt-fault-draft", 32).await?;
        let issued = fixture.issue("receipt-fault-caller").await?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let headers = headers(
            &fixture,
            issued.secret.as_deref().context("caller secret")?,
            &revision.revision_id,
        )?;
        let app = Router::new()
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(axum::middleware::from_fn_with_state(
                fixture.service.clone(),
                crate::tenancy::http::dispatch,
            ));
        let ingress = Listener::start(app).await?;
        Ok(Self {
            fixture,
            provider,
            ingress,
            headers,
            protocol,
        })
    }
    pub(super) fn request(
        &self,
        client: &reqwest::Client,
        stream: bool,
    ) -> reqwest::RequestBuilder {
        let body = match self.protocol {
            Protocol::Chat => {
                json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":stream})
            }
            Protocol::Responses => {
                json!({"model":MODEL,"input":"hello","stream":stream,"store":true})
            }
        };
        client
            .post(format!("{}{}", self.ingress.base, self.protocol.path()))
            .headers(self.headers.clone())
            .json(&body)
    }
    pub(super) async fn receipt(&self, id: &ExecutionId) -> Result<ExecutionReceipt> {
        store::lookup(
            &self.fixture.db,
            &self.fixture.installation,
            &self.fixture.tenant,
            id,
        )
        .await?
        .context("durable actual receipt")
    }
    pub(super) async fn count(&self, table: &str) -> Result<i64> {
        // All callers pass fixed test-authored table identifiers.
        Ok(self
            .fixture
            .db
            .query_one(Statement::from_string(
                self.fixture.db.get_database_backend(),
                format!("SELECT COUNT(*) AS n FROM {table}"),
            ))
            .await?
            .context("count")?
            .try_get("", "n")?)
    }
    pub(super) async fn close(self) -> Result<()> {
        self.provider.open.cancel();
        self.provider.tail.cancel();
        let ingress = self.ingress.close().await;
        let provider = self.provider.close().await;
        ingress?;
        provider?;
        Ok(())
    }
}
pub(super) fn modes() -> &'static [bool] {
    if cfg!(feature = "http2") {
        &[false, true]
    } else {
        &[false]
    }
}
pub(super) fn client(h2: bool) -> Result<reqwest::Client> {
    super::super::Harness::client(h2)
}
pub(super) fn identity(response: &reqwest::Response, h2: bool) -> Result<ExecutionId> {
    ensure!(
        response.version()
            == if h2 {
                ::http::Version::HTTP_2
            } else {
                ::http::Version::HTTP_11
            }
    );
    response
        .headers()
        .get("x-bitrouter-request-id")
        .context("admitted execution identity")?
        .to_str()?
        .try_into()
        .map_err(Into::into)
}
pub(super) fn completed(receipt: &ExecutionReceipt) -> Result<()> {
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Completed);
    ensure!(receipt.usage.input_tokens == Some(11) && receipt.usage.output_tokens == Some(7));
    ensure!(receipt.attempts[0].target.is_some());
    Ok(())
}
pub(super) struct Release(pub(super) Service);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.cleanup.release_gate();
    }
}
