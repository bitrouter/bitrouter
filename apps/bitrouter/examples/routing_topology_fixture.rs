//! Cargo-built HTTP provider and production gateway assembly for composition tests.
use anyhow::{Context, Result};
use axum::{
    Json,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{fs::OpenOptions, io::Write, path::PathBuf, sync::Arc};

#[derive(Clone)]
struct Journal(PathBuf);
impl Journal {
    fn write(&self, value: Value) -> Result<()> {
        let mut file = OpenOptions::new().create(true).append(true).open(&self.0)?;
        let mut line = serde_json::to_vec(&value)?;
        line.push(b'\n');
        file.write_all(&line)?;
        Ok(())
    }
}
struct Closed(Journal, String);
impl Drop for Closed {
    fn drop(&mut self) {
        if let Err(error) = self.0.write(json!({"closed": self.1})) {
            eprintln!("fixture close journal: {error}");
        }
    }
}
async fn provider(
    State(journal): State<Journal>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let observed: std::collections::BTreeMap<_, _> = headers
        .iter()
        .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str(), value)))
        .collect();
    if let Err(error) = journal.write(json!({"body": body, "headers": observed})) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            error.to_string(),
        )
            .into_response();
    }
    let text = body["messages"][0]["content"].as_str().unwrap_or("");
    if text == "reject" {
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "fixture throttled",
        )
            .into_response();
    }
    if text == "slow" {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    if body["stream"].as_bool() == Some(true) {
        let hold = text == "hold";
        let stream = async_stream::stream! {
            let _closed = Closed(journal, if hold {"hold"} else {"hello"}.into());
            yield Ok::<_, std::io::Error>("data: {\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"model\":\"selected\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ready\"},\"finish_reason\":null}]}\n\n");
            if hold {std::future::pending::<()>().await;}
            yield Ok("data: {\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"model\":\"selected\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":1,\"total_tokens\":5}}\n\n");
            yield Ok("data: [DONE]\n\n");
        };
        return match Response::builder()
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from_stream(stream))
        {
            Ok(response) => response,
            Err(error) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                error.to_string(),
            )
                .into_response(),
        };
    }
    Json(json!({"id":"fixture","object":"chat.completion","model":"selected","choices":[{"index":0,"message":{"role":"assistant","content":"ready"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":1,"total_tokens":5}})).into_response()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).context("fixture mode")?;
    if mode == "capabilities" {
        println!(
            "{}",
            json!({"semantic": cfg!(feature="semantic-routing"), "inference": cfg!(feature="inference-routing"), "core": cfg!(feature="core"), "harness": cfg!(feature="harness")})
        );
        return Ok(());
    }
    let listen = args.get(2).context("fixture listen address")?;
    let path = PathBuf::from(args.get(3).context("fixture configuration or journal")?);
    if mode == "provider" {
        let app = axum::Router::new()
            .route("/health", get(|| async { "ready" }))
            .route("/chat/completions", post(provider))
            .with_state(Journal(path));
        axum::serve(tokio::net::TcpListener::bind(listen).await?, app).await?;
    } else {
        anyhow::ensure!(mode == "gateway", "unknown fixture mode");
        let config = bitrouter::config::load(&path).await?;
        let assembled = bitrouter::assemble::build_app_with_path(&config, Some(&path)).await?;
        if !config.server.skip_auth {
            bitrouter::auth::db::upsert_user(&assembled.db, "topology-user").await?;
            let key = bitrouter::auth::keys::generate();
            bitrouter::auth::db::insert_api_key(
                &assembled.db,
                &bitrouter::auth::db::NewApiKey {
                    id: "topology-key".into(),
                    key_hash: key.hash,
                    user_id: "topology-user".into(),
                    spend_limit_micro_usd: None,
                    rpm_limit: None,
                    policy_id: None,
                },
            )
            .await?;
            // The parent reads this inside its private temporary directory; never log credentials.
            std::fs::write(path.with_extension("key"), key.secret)?;
        }
        let app = Arc::new(assembled.app);
        let journal = Journal(path.with_extension("ingress.jsonl"));
        app.serve_with_router_wrapper(listen, move |router| {
            let journal = journal.clone();
            router.layer(axum::middleware::from_fn(move |request: axum::extract::Request, next: axum::middleware::Next| {
                let journal = journal.clone();
                async move {
                    if let Err(error) = journal.write(json!({"timeout":request.headers().get("x-bitrouter-timeout-ms").and_then(|v| v.to_str().ok())})) {eprintln!("gateway fixture journal: {error}");}
                    next.run(request).await
                }
            }))
        }).await?;
    }
    Ok(())
}
