use std::{collections::VecDeque, io, sync::Arc};

use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, Request, Version},
    response::Response,
};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::Value;
use tokio::{net::TcpListener, sync::Mutex, task::JoinSet};

use super::TestResult;

pub(super) struct Recorded {
    pub connection: usize,
    pub version: Version,
    pub method: String,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

pub(super) struct Server {
    pub endpoint: String,
    pub records: Arc<Mutex<Vec<Recorded>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        // Dropping the accept task also drops its JoinSet, aborting all sockets.
        self.task.abort();
    }
}

pub(super) fn json(value: Value) -> TestResult<Response> {
    Ok(Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))?)
}

pub(super) async fn serve(version: Version, replies: Vec<Option<Response>>) -> TestResult<Server> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let records = Arc::new(Mutex::new(Vec::new()));
    let captured = records.clone();
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let task = tokio::spawn(async move {
        let mut sockets = JoinSet::new();
        let mut connection = 0;
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (socket, _) = match accepted {
                        Ok(accepted) => accepted,
                        Err(error) => {
                            eprintln!("fixture accept failed: {error}");
                            return;
                        }
                    };
                    connection += 1;
                    let records = captured.clone();
                    let replies = replies.clone();
                    sockets.spawn(async move {
                        let service = service_fn(move |request: Request<Incoming>| {
                            let records = records.clone();
                            let replies = replies.clone();
                            async move {
                                let (parts, body) = request.into_parts();
                                let body = to_bytes(Body::new(body), 1024 * 1024)
                                    .await.map_err(io::Error::other)?;
                                records.lock().await.push(Recorded {
                                    connection,
                                    version: parts.version,
                                    method: parts.method.to_string(),
                                    path: parts.uri.path_and_query()
                                        .map_or("/", |value| value.as_str()).to_owned(),
                                    headers: parts.headers,
                                    body: body.to_vec(),
                                });
                                // None simulates losing the response after accepting
                                // the entire request body, with an ambiguous outcome.
                                replies.lock().await.pop_front().flatten()
                                    .ok_or_else(|| io::Error::other("fixture response lost"))
                            }
                        });
                        // Use explicit Hyper servers so HTTP/1 and h2c cannot be
                        // confused by a test server's automatic protocol selection.
                        // https://docs.rs/hyper/latest/hyper/server/conn/index.html
                        let result = if version == Version::HTTP_2 {
                            hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                                .serve_connection(TokioIo::new(socket), service).await
                        } else {
                            hyper::server::conn::http1::Builder::new()
                                .serve_connection(TokioIo::new(socket), service).await
                        };
                        if let Err(error) = result {
                            // Protocol mismatch and lost-response tests intentionally
                            // cause connection errors; successful calls assert output.
                            eprintln!("fixture connection ended: {error}");
                        }
                    });
                }
                joined = sockets.join_next(), if !sockets.is_empty() => {
                    if let Some(Err(error)) = joined {
                        eprintln!("fixture task failed: {error}");
                        return;
                    }
                }
            }
        }
    });
    Ok(Server {
        endpoint,
        records,
        task,
    })
}
