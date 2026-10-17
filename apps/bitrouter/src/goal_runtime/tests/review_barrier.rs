//! Hold the first independent review response until the new Goal version commits.
use super::*;
use axum::body::Bytes;
use axum::extract::OriginalUri;
use axum::http::StatusCode;
use axum::response::IntoResponse;

pub(super) struct ReviewBarrier {
    pub uri: String,
    pub entered: Arc<tokio::sync::Notify>,
    pub release: Arc<tokio::sync::Notify>,
    server: JoinHandle<()>,
}

impl Drop for ReviewBarrier {
    fn drop(&mut self) {
        self.release.notify_one();
        self.server.abort();
    }
}

impl ReviewBarrier {
    pub async fn start(backend: String) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let uri = format!("http://{}", listener.local_addr()?);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let handler_entered = entered.clone();
        let handler_release = release.clone();
        let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let client = reqwest::Client::new();
        let router = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |OriginalUri(path): OriginalUri, body: Bytes| {
                let backend = backend.clone();
                let entered = handler_entered.clone();
                let release = handler_release.clone();
                let first = first.clone();
                let client = client.clone();
                async move {
                    let response = async {
                        let url = format!("{backend}{path}");
                        let request = wiremock::Request {
                            url: url.parse()?,
                            method: axum::http::Method::POST,
                            headers: Default::default(),
                            body: body.to_vec(),
                        };
                        let independent_review =
                            provider::model_state(&request).is_some_and(|state| {
                                state.get("actual_workspace").is_some()
                                    && state["task"]["id"] == "right"
                            });
                        let response = client.post(url).body(body).send().await?;
                        let status = response.status();
                        let bytes = response.bytes().await?;
                        if independent_review
                            && first.swap(false, std::sync::atomic::Ordering::SeqCst)
                        {
                            entered.notify_one();
                            release.notified().await;
                        }
                        Ok::<_, anyhow::Error>((
                            status,
                            [("content-type", "application/json")],
                            bytes,
                        ))
                    }
                    .await;
                    match response {
                        Ok(response) => response.into_response(),
                        Err(error) => (StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
                    }
                }
            }),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self {
            uri,
            entered,
            release,
            server,
        })
    }
}
