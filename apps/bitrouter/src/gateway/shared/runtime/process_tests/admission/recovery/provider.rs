//! Controlled provider work survives HTTP disconnect until explicitly ended.

#[path = "provider/slow_reader.rs"]
mod slow_reader;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    response::{IntoResponse, Response},
};
use futures::StreamExt;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum End {
    Complete,
    Terminate,
}

pub(super) struct Receipt {
    pub(super) execution: String,
    pub(super) end: End,
    pub(super) reference: String,
}

pub(super) struct Arrival {
    pub(super) execution: String,
    pub(super) tag: String,
    pub(super) path: String,
    pub(super) authorization: String,
    pub(super) streaming: bool,
    release: oneshot::Sender<End>,
    receipt: oneshot::Receiver<Receipt>,
}

impl Arrival {
    pub(super) async fn end(self, end: End) -> Result<Receipt> {
        self.release
            .send(end)
            .map_err(|_| anyhow::anyhow!("provider operation ended before control"))?;
        let receipt = tokio::time::timeout(Duration::from_secs(3), self.receipt)
            .await
            .context("provider did not acknowledge termination/completion")??;
        ensure!(receipt.execution == self.execution && receipt.end == end);
        Ok(receipt)
    }
}

#[derive(Clone)]
struct StateData {
    arrivals: mpsc::Sender<Result<Arrival>>,
    calls: Arc<AtomicUsize>,
    stop: CancellationToken,
    // Operations are owned independently of the HTTP request/body. Killing a
    // gateway cannot masquerade as a receipt that its provider work stopped.
    operations: Arc<Mutex<JoinSet<()>>>,
}

pub(super) struct Provider {
    pub(super) base: String,
    state: StateData,
    arrivals: mpsc::Receiver<Result<Arrival>>,
    roots: JoinSet<std::io::Result<()>>,
}

impl Provider {
    pub(super) async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let (sender, arrivals) = mpsc::channel(16);
        let state = StateData {
            arrivals: sender,
            calls: Arc::default(),
            stop: CancellationToken::new(),
            operations: Arc::new(Mutex::new(JoinSet::new())),
        };
        let app = Router::new().fallback(request).with_state(state.clone());
        let stop = state.stop.clone();
        let mut roots = JoinSet::new();
        roots.spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
        });
        Ok(Self {
            base,
            state,
            arrivals,
            roots,
        })
    }

    pub(super) fn calls(&self) -> usize {
        self.state.calls.load(Ordering::SeqCst)
    }

    pub(super) async fn arrival(
        &mut self,
        tag: &str,
        account: &str,
        streaming: bool,
    ) -> Result<Arrival> {
        let arrival = tokio::time::timeout(Duration::from_secs(5), self.arrivals.recv())
            .await
            .context("stock gateway never reached controlled provider")?
            .context("provider arrivals closed")??;
        ensure!(
            arrival.tag == tag,
            "unexpected dispatch: {} instead of {tag}",
            arrival.tag
        );
        ensure!(arrival.path == format!("/{account}/chat/completions"));
        let secret = match account {
            "tenant" => SECRETS[0],
            "account" => SECRETS[1],
            "other" => SECRETS[2],
            _ => anyhow::bail!("unknown fixture account"),
        };
        ensure!(
            arrival.authorization == format!("Bearer {secret}"),
            "wrong provider credential"
        );
        ensure!(arrival.streaming == streaming);
        uuid::Uuid::parse_str(&arrival.execution)?;
        Ok(arrival)
    }

    // Gateways are stopped first, so the joined roots close the finite window
    // covered by the exact HTTP-entry count. No sleep is used to infer absence.
    pub(super) async fn finish(mut self, expected: Option<usize>) -> Result<()> {
        self.state.stop.cancel();
        let mut cleanup = Vec::new();
        let mut operations = self.state.operations.lock().await;
        let joined = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(result) = operations.join_next().await {
                result.context("provider operation task failed")?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("provider operations did not join")
        .and_then(|result| result);
        if joined.is_err() {
            operations.abort_all();
            while let Some(result) = operations.join_next().await {
                if let Err(error) = result
                    && !error.is_cancelled()
                {
                    cleanup.push(Err(error.into()));
                }
            }
        }
        cleanup.push(joined);
        drop(operations);
        match tokio::time::timeout(Duration::from_secs(3), self.roots.join_next()).await {
            Ok(Some(result)) => cleanup.push(
                result
                    .context("provider root panicked")
                    .and_then(|r| r.map_err(Into::into)),
            ),
            Ok(None) => cleanup.push(Err(anyhow::anyhow!("missing provider root"))),
            Err(error) => {
                cleanup.push(Err(error).context("provider serving root did not join"));
                self.roots.abort_all();
                while let Some(result) = self.roots.join_next().await {
                    if let Err(error) = result
                        && !error.is_cancelled()
                    {
                        cleanup.push(Err(error.into()));
                    }
                }
            }
        }
        if let Some(expected) = expected {
            cleanup.push(if self.calls() == expected {
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "provider received {} HTTP entries; expected {expected}",
                    self.calls()
                ))
            });
            // An unexpected parsed dispatch is evidence even if its caller was
            // cancelled during teardown. Malformed entries are counted too.
            if self.arrivals.try_recv().is_ok() {
                cleanup.push(Err(anyhow::anyhow!("unconsumed provider arrival")));
            }
        }
        combine_results(Ok(()), cleanup)
    }
}

async fn request(State(state): State<StateData>, request: Request) -> Response {
    // Count before method/path/header/JSON parsing; bad extra calls cannot hide.
    let count = state.calls.fetch_add(1, Ordering::SeqCst) + 1;
    if count > 16 {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    match prepare(&state, request).await {
        Ok(response) => response,
        Err(error) => {
            let _ = state.arrivals.try_send(Err(error));
            StatusCode::BAD_REQUEST.into_response()
        }
    }
}

async fn prepare(state: &StateData, request: Request) -> Result<Response> {
    let (parts, body) = request.into_parts();
    ensure!(parts.method == axum::http::Method::POST);
    let bytes = tokio::time::timeout(Duration::from_secs(2), to_bytes(body, 32 * 1024)).await??;
    let body: Value = serde_json::from_slice(&bytes)?;
    let header = |name: &str| -> Result<String> {
        Ok(parts
            .headers
            .get(name)
            .context("missing provider header")?
            .to_str()?
            .to_owned())
    };
    let execution = header("x-bitrouter-request-id")?;
    let tag = body["messages"][0]["content"]
        .as_str()
        .context("missing fixture tag")?
        .to_owned();
    let streaming = body.get("stream").and_then(Value::as_bool) == Some(true);
    let (release, command) = oneshot::channel();
    let (receipt, settled) = oneshot::channel();
    let (output, result) = oneshot::channel();
    let operation_execution = execution.clone();
    let stop = state.stop.clone();
    let mut operations = state.operations.lock().await;
    ensure!(!state.stop.is_cancelled(), "provider is stopping");
    operations.spawn(async move {
        let end = tokio::select! {
            () = stop.cancelled() => None,
            end = command => end.ok(),
        };
        if let Some(end) = end {
            let reference = format!("fixture-provider-{}", uuid::Uuid::new_v4());
            // Work has ended at this point. Socket availability is deliberately
            // irrelevant to this out-of-band provider receipt.
            let _ = output.send(end);
            let _ = receipt.send(Receipt {
                execution: operation_execution,
                end,
                reference,
            });
        }
    });
    drop(operations);
    let arrival = Arrival {
        execution,
        tag,
        path: parts.uri.path().to_owned(),
        authorization: header("authorization")?,
        streaming,
        release,
        receipt: settled,
    };
    state
        .arrivals
        .try_send(Ok(arrival))
        .map_err(|_| anyhow::anyhow!("provider arrival queue full/closed"))?;
    if streaming {
        let first = futures::stream::once(async {
            Ok::<_, std::io::Error>("data: {\"id\":\"fixture-stream\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"held\"},\"finish_reason\":null}]}\n\n".to_owned())
        });
        let last = futures::stream::once(async move {
            match result.await {
                Ok(End::Complete) => Ok("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\ndata: [DONE]\n\n".to_owned()),
                _ => Err(std::io::Error::other("controlled provider terminated without terminal evidence")),
            }
        });
        Ok((
            [("content-type", "text/event-stream")],
            Body::from_stream(first.chain(last)),
        )
            .into_response())
    } else {
        match result.await {
            Ok(End::Complete) => Ok(axum::Json(json!({"id":"fixture-json","object":"chat.completion",
                "created":1,"model":"mock","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}})).into_response()),
            _ => Ok(StatusCode::SERVICE_UNAVAILABLE.into_response()),
        }
    }
}
