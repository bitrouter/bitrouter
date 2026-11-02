use super::*;
use axum::body::Bytes;
use std::net::SocketAddr;
use tokio::{net::TcpListener, sync::Notify};

pub(super) struct Listener {
    pub address: SocketAddr,
    stop: CancellationToken,
    task: AbortOnDropHandle<std::io::Result<()>>,
}
impl Listener {
    pub(super) async fn start(router: Router) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
        }));
        Ok(Self {
            address,
            stop,
            task,
        })
    }
    pub(super) async fn close(self) -> Result<()> {
        self.stop.cancel();
        tokio::time::timeout(TIMEOUT, self.task).await???;
        Ok(())
    }
}
#[derive(Clone)]
pub(super) struct Provider {
    pub calls: Arc<AtomicUsize>,
    pub received: Arc<Notify>,
    pub headers: CancellationToken,
    pub tail: CancellationToken,
    pub dropped: CancellationToken,
}
impl Provider {
    pub(super) fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            received: Arc::new(Notify::new()),
            headers: CancellationToken::new(),
            tail: CancellationToken::new(),
            dropped: CancellationToken::new(),
        }
    }
    pub(super) async fn start(&self) -> Result<Listener> {
        Listener::start(
            Router::new()
                .route("/v1/chat/completions", post(provider))
                .with_state(self.clone()),
        )
        .await
    }
}
struct Dropped(CancellationToken);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
async fn provider(State(state): State<Provider>, Json(body): Json<serde_json::Value>) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    if body["model"] != "model-a" || body["stream"] != true {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let dropped = Dropped(state.dropped.clone());
    state.received.notify_one();
    state.headers.cancelled().await;
    // https://platform.openai.com/docs/api-reference/chat/streaming
    let first = json!({"id":"socket-stream","object":"chat.completion.chunk","created":0,"model":"model-a",
        "choices":[{"index":0,"delta":{"content":"socket-prefix"},"finish_reason":null}]});
    let last = json!({"id":"socket-stream","object":"chat.completion.chunk","created":0,"model":"model-a",
        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}});
    let body = async_stream::stream! {
        let _dropped = dropped;
        yield Ok::<_, std::io::Error>(Bytes::from(format!("data: {first}\n\n")));
        state.tail.cancelled().await;
        yield Ok(Bytes::from(format!("data: {last}\n\ndata: [DONE]\n\n")));
    };
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(body),
    )
        .into_response()
}
