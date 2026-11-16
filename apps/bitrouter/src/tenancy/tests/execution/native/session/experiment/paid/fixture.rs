use super::*;
use crate::core::evolution::rubric::RubricEvaluation;
use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use std::sync::Mutex;
use tokio::{net::TcpListener, sync::mpsc};

#[derive(Clone)]
struct JudgeState {
    answer: Arc<Mutex<Option<RubricEvaluation>>>,
    release: CancellationToken,
    requests: mpsc::Sender<Value>,
}
async fn judge(
    State(state): State<JudgeState>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if state.requests.try_send(body).is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    state.release.cancelled().await;
    let answer = state
        .answer
        .lock()
        .ok()
        .and_then(|v| v.as_ref().and_then(|a| serde_json::to_string(a).ok()));
    let Some(answer) = answer else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    Json(json!({"id":"actual-native-judge","object":"chat.completion","created":1,"model":"judge-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":answer},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6,
            "prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
            "completion_tokens_details":{"reasoning_tokens":0}}
    })).into_response()
}
pub(super) struct Judge {
    pub url: String,
    pub release: CancellationToken,
    answer: Arc<Mutex<Option<RubricEvaluation>>>,
    requests: mpsc::Receiver<Value>,
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}
impl Judge {
    pub async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let (send, requests) = mpsc::channel(4);
        let answer = Arc::new(Mutex::new(None));
        let release = CancellationToken::new();
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let router = Router::new()
            .route("/v1/chat/completions", post(judge))
            .with_state(JudgeState {
                answer: answer.clone(),
                release: release.clone(),
                requests: send,
            });
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });
        Ok(Self {
            url,
            release,
            answer,
            requests,
            stop,
            task: Some(task),
        })
    }
    pub fn retained_answer(&self, evaluation: RubricEvaluation) -> Result<()> {
        *self
            .answer
            .lock()
            .map_err(|_| anyhow::anyhow!("judge answer poisoned"))? = Some(evaluation);
        Ok(())
    }
    pub async fn received(&mut self) -> Result<Value> {
        tokio::time::timeout(TIMEOUT, self.requests.recv())
            .await?
            .context("actual paid request missing")
    }
    pub fn extra_request(&mut self) -> bool {
        self.requests.try_recv().is_ok()
    }
    pub async fn finish(mut self) -> Result<()> {
        self.release.cancel();
        self.stop.cancel();
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(TIMEOUT, &mut task).await {
                Ok(result) => result??,
                Err(error) => {
                    task.abort();
                    let reaped = tokio::time::timeout(TIMEOUT, task).await;
                    ensure!(reaped.is_ok(), "judge listener task did not terminate");
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }
}
impl Drop for Judge {
    fn drop(&mut self) {
        self.release.cancel();
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
