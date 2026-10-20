//! Bounded shared-mode HTTP/1 transport, separate from execution custody.
//!
//! Each connection serves one request. Hyper's DATA threshold does not bound
//! accumulated response heads on a persistent, pipelined connection. Disabling
//! reuse also prevents an unread rejected body from admitting another request.
//! Local mode keeps its existing transport.

mod body;
mod io;
pub(super) mod owned;

#[cfg(test)]
mod tests;

use std::convert::Infallible;
#[cfg(test)]
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body::Body as _;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::config::gateway_state::SharedGatewayConfig;

use self::body::{Ingress, Output};
use self::io::WriteDeadline;

// This is a buffering threshold, not an exact allocation/RSS bound. Hyper can
// retain read capacity and queue a final frame/framing beyond its threshold.
const CHUNK_BYTES: usize = 65_536;
const HEADER_VALUES: usize = 100;
const METADATA_BYTES: usize = 65_536;

#[derive(Clone, Copy)]
pub(super) struct Limits {
    connections: usize,
    request_bytes: usize,
    header_timeout: Duration,
    request_timeout: Duration,
    write_timeout: Duration,
    #[cfg(test)]
    drain_timeout: Duration,
}

impl From<&SharedGatewayConfig> for Limits {
    fn from(config: &SharedGatewayConfig) -> Self {
        Self {
            connections: config.max_connections,
            request_bytes: config.max_request_bytes,
            header_timeout: Duration::from_millis(config.header_timeout_ms),
            request_timeout: Duration::from_millis(config.request_timeout_ms),
            write_timeout: Duration::from_millis(config.write_stall_timeout_ms),
            #[cfg(test)]
            drain_timeout: Duration::from_millis(config.drain_timeout_ms),
        }
    }
}

impl Limits {
    pub fn for_operator(config: &SharedGatewayConfig, connections: usize) -> Self {
        Self {
            connections,
            request_bytes: 2 * 1024 * 1024,
            ..Self::from(config)
        }
    }
}

/// Fixture compatibility wrapper. Production supplies one process deadline to
/// the retained transport owner instead of creating a relative drain timeout.
#[cfg(test)]
pub(super) async fn serve<S>(
    listener: TcpListener,
    router: Router,
    limits: Limits,
    shutdown: S,
) -> std::io::Result<()>
where
    S: Future<Output = ()>,
{
    let owner = owned::Transport::start(listener, router, limits).map_err(std::io::Error::other)?;
    tokio::pin!(shutdown);
    tokio::select! {
        () = &mut shutdown => {},
        result = owner.wait_driver() => { result.map_err(std::io::Error::other)?; },
    }
    let mut report = owner
        .drain_until(Instant::now() + limits.drain_timeout)
        .await
        .map_err(std::io::Error::other)?;
    if !report.is_quiescent() {
        owner.request_force();
        report = owner.join_forced().await.map_err(std::io::Error::other)?;
    }
    if report.driver.failed || report.driver.panicked {
        return Err(std::io::Error::other("shared HTTP accept driver failed"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum IngressState {
    Headers,
    Body(Instant),
    Complete,
}

async fn ingress_expired(mut state: watch::Receiver<IngressState>) {
    loop {
        let current = *state.borrow_and_update();
        match current {
            IngressState::Body(deadline) => {
                tokio::select! {
                    () = tokio::time::sleep_until(deadline) => return,
                    changed = state.changed() => {
                        if changed.is_err() {
                            std::future::pending::<()>().await;
                        }
                    }
                }
            }
            IngressState::Complete => std::future::pending::<()>().await,
            IngressState::Headers => {
                if state.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        }
    }
}

async fn connection<I>(socket: I, router: Router, limits: Limits, stop: CancellationToken)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (ingress, observed) = watch::channel(IngressState::Headers);
    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
        let router = router.clone();
        let ingress = ingress.clone();
        async move {
            // Reject a known oversized length without letting a collector use
            // it as an allocation hint or entering application admission.
            if request.body().size_hint().lower() > limits.request_bytes as u64 {
                return Ok::<_, Infallible>(bounded_response(
                    StatusCode::PAYLOAD_TOO_LARGE.into_response(),
                ));
            }
            let request = request.map(|body| {
                Body::new(Ingress::new(
                    body,
                    limits.request_bytes,
                    ingress,
                    limits.request_timeout,
                ))
            });
            let response = if request.method() == Method::CONNECT {
                StatusCode::METHOD_NOT_ALLOWED.into_response()
            } else {
                // Request admission belongs inside the Router before consuming
                // extractors. Router readiness alone is not admission control.
                match router.oneshot(request).await {
                    Ok(response) => response,
                    Err(error) => match error {},
                }
            };
            Ok::<_, Infallible>(bounded_response(response))
        }
    });
    // Official HTTP/1 builder contract:
    // https://docs.rs/hyper/1.10.1/hyper/server/conn/http1/struct.Builder.html
    let mut builder = http1::Builder::new();
    builder
        .keep_alive(false)
        .pipeline_flush(false)
        .max_buf_size(CHUNK_BYTES)
        .max_headers(HEADER_VALUES)
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_timeout);
    let connection = builder.serve_connection(
        TokioIo::new(WriteDeadline::new(socket, limits.write_timeout)),
        service,
    );
    tokio::pin!(connection);
    tokio::select! {
        result = &mut connection => trace_connection(result),
        () = ingress_expired(observed) => {},
        () = stop.cancelled() => {
            connection.as_mut().graceful_shutdown();
            // The serving owner enforces the total drain deadline and joins
            // cancellation. Do not timeout healthy inference during serving.
            trace_connection(connection.await);
        }
    }
}

fn trace_connection(result: Result<(), hyper::Error>) {
    if let Err(error) = result {
        tracing::debug!(%error, "shared HTTP connection closed");
    }
}

fn bounded_response(mut response: Response) -> Response<Output> {
    if response.status() == StatusCode::SWITCHING_PROTOCOLS
        || !body::valid_metadata(response.headers())
    {
        response = StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    // A custom reason phrase can bypass header size accounting.
    response
        .extensions_mut()
        .remove::<hyper::ext::ReasonPhrase>();
    response.map(Output::new)
}
