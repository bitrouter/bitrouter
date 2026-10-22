//! Cleartext protocol selection and connection-owned HTTP/2 streams.
//!
//! Stream futures are polled inside the retained connection task, never spawned.
//! Aborting/joining that task therefore also drops every handler and body owner.

mod compression;
mod input;
mod preface;
#[cfg(test)]
mod tests;
mod wire;

use std::future::poll_fn;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use h2::{Reason, SendStream, server::SendResponse};
use http::header;
use tokio::time::timeout;
use tokio::time::timeout_at;

use super::*;

const MAX_STREAMS: u32 = 128;

pub(super) async fn accept<I>(
    socket: I,
    router: Router,
    limits: Limits,
    stop: CancellationToken,
    slots: Arc<Semaphore>,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let deadline = Instant::now() + limits.header_timeout;
    let detected = tokio::select! {
        () = stop.cancelled() => return,
        result = timeout_at(deadline, preface::detect(socket)) => result,
    };
    let Ok(Ok((http2, socket))) = detected else {
        return;
    };
    if !http2 {
        let limits = Limits {
            header_timeout: deadline.saturating_duration_since(Instant::now()),
            ..limits
        };
        connection(socket, router, limits, stop).await;
        return;
    }
    if let Err(error) = serve(socket, router, limits, stop, slots, deadline).await {
        tracing::debug!(%error, "shared HTTP/2 connection closed");
    }
}

async fn serve<I>(
    socket: I,
    router: Router,
    limits: Limits,
    stop: CancellationToken,
    slots: Arc<Semaphore>,
    deadline: Instant,
) -> Result<()>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Explicit bounds on decoded headers, receive windows, send buffering and
    // reset bookkeeping. Keep adaptive/unbounded window growth disabled.
    // https://docs.rs/h2/latest/h2/server/struct.Builder.html
    let mut builder = h2::server::Builder::new();
    builder
        .max_concurrent_streams(MAX_STREAMS.min(limits.requests as u32))
        .initial_window_size(CHUNK_BYTES as u32)
        .initial_connection_window_size(1024 * 1024)
        .max_header_list_size(METADATA_BYTES as u32)
        .max_send_buffer_size(CHUNK_BYTES)
        .max_concurrent_reset_streams(128)
        .max_pending_accept_reset_streams(20)
        .max_local_error_reset_streams(Some(128));
    let headers = Arc::new(wire::Headers::default());
    let socket = wire::Wire::new(socket, headers.clone(), deadline, limits.header_timeout);
    let mut connection = tokio::select! {
        () = stop.cancelled() => return Ok(()),
        result = timeout_at(deadline,
            builder.handshake::<_, Bytes>(WriteDeadline::new(socket, limits.write_timeout))) => result??,
    };
    let mut streams = FuturesUnordered::new();
    let mut draining = false;
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled(), if !draining => {
                draining = true;
                connection.graceful_shutdown();
            }
            () = headers.exhausted.cancelled() => anyhow::bail!("HTTP/2 response metadata budget exhausted"),
            Some(()) = streams.next(), if !streams.is_empty() => {}
            accepted = connection.accept() => {
                let Some(accepted) = accepted else { break; };
                let (request, response) = accepted?;
                // Bound application work across all HTTP/2 connections. No
                // waiting queue is created before the Router's own admission.
                let permit = if draining { None } else { slots.clone().try_acquire_owned().ok() };
                let router = router.clone();
                let headers = headers.clone();
                streams.push(async move {
                    let admitted = permit.is_some();
                    let _permit = permit;
                    if let Err(error) = stream(request, response, router, limits, headers, admitted).await {
                        tracing::debug!(%error, "shared HTTP/2 stream closed");
                    }
                });
            }
        }
    }
    // Includes connection errors and forced cancellation: no detached services.
    drop(streams);
    Ok(())
}

async fn stream(
    request: Request<h2::RecvStream>,
    mut reply: SendResponse<Bytes>,
    router: Router,
    limits: Limits,
    headers: Arc<wire::Headers>,
    admitted: bool,
) -> Result<()> {
    let id = reply.stream_id().as_u32();
    let head = request.method() == Method::HEAD;
    let (ingress, observed) = watch::channel(IngressState::Headers);
    let service = async {
        if !admitted {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        if !body::valid_metadata(request.headers()) {
            return StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE.into_response();
        }
        let length = request
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if length.is_some_and(|length| length > limits.request_bytes as u64) {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        if request.method() == Method::CONNECT {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        let request = request.map(|body| {
            Body::new(Ingress::new(
                input::Input::new(body, length),
                limits.request_bytes,
                ingress,
                limits.request_timeout,
            ))
        });
        match router.oneshot(request).await {
            Ok(response) => response,
            Err(error) => match error {},
        }
    };
    let response = tokio::select! {
        _ = poll_fn(|cx| reply.poll_reset(cx)) => return Ok(()),
        () = ingress_expired(observed.clone()) => StatusCode::REQUEST_TIMEOUT.into_response(),
        response = service => response,
    };
    let (mut parts, mut body) = bounded_response(response).into_parts();
    // HTTP/2 carries the same HTTP semantics but forbids connection-specific
    // fields and HTTP/1 chunked framing. Remove nominated hop-by-hop fields too.
    // https://httpwg.org/specs/rfc9113.html#ConnectionSpecific
    let nominated: Vec<_> = parts
        .headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(',').map(str::trim))
        .filter_map(|name| http::HeaderName::from_bytes(name.as_bytes()).ok())
        .collect();
    for name in nominated {
        parts.headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "upgrade",
    ] {
        parts.headers.remove(name);
    }
    compression::never_index(&mut parts.headers);
    let end = head || body.is_end_stream();
    anyhow::ensure!(
        headers.reserve(id, &parts.headers),
        "HTTP/2 metadata budget exhausted"
    );
    let mut output = match reply.send_response(http::Response::from_parts(parts, ()), end) {
        Ok(output) => output,
        Err(error) => {
            headers.unsent(id);
            return Err(error.into());
        }
    };
    let result = if end {
        Ok(())
    } else {
        tokio::select! {
            () = ingress_expired(observed) => Err(anyhow::anyhow!("HTTP/2 body ingress timed out")),
            result = send_body(&mut body, &mut output, limits.write_timeout, &headers, id) => result,
        }
    };
    drop(body);
    if result.is_err() {
        output.send_reset(Reason::CANCEL);
    }
    // Retain reset observation even for empty/overload responses and queued
    // trailers. This future remains owned by the connection, with no new task.
    finish_headers(&mut output, &headers, id).await;
    result
}

async fn finish_headers(output: &mut SendStream<Bytes>, headers: &wire::Headers, id: u32) {
    tokio::select! {
        () = headers.released(id) => {},
        _ = poll_fn(|cx| output.poll_reset(cx)) => {
            headers.cancel(id);
            // Do not poll an already-ready reset again. The connection's next
            // successful encoder flush will reclaim discarded reservations.
            headers.released(id).await;
        }
    }
}

async fn send_body(
    body: &mut Output,
    output: &mut SendStream<Bytes>,
    stall: Duration,
    headers: &wire::Headers,
    id: u32,
) -> Result<()> {
    loop {
        // Waiting for the application/provider to produce a frame is not a
        // network stall and must not impose an inference duration limit.
        let frame = tokio::select! {
            _ = poll_fn(|cx| output.poll_reset(cx)) => return Ok(()),
            frame = poll_fn(|cx| std::pin::Pin::new(&mut *body).poll_frame(cx)) => frame,
        };
        let Some(frame) = frame else {
            output.send_data(Bytes::new(), true)?;
            return Ok(());
        };
        let frame = frame.map_err(|error| anyhow::anyhow!(error))?;
        match frame.into_data() {
            Ok(mut data) => {
                while !data.is_empty() {
                    output.reserve_capacity(data.len());
                    // Wait for actual stream AND connection flow-control
                    // capacity; never enqueue unreserved DATA in h2.
                    // https://docs.rs/h2/latest/h2/struct.SendStream.html#method.reserve_capacity
                    let capacity = timeout(stall, async {
                        loop {
                            let capacity = poll_fn(|cx| output.poll_capacity(cx))
                                .await
                                .context("HTTP/2 send stream closed")??;
                            if capacity > 0 {
                                return Ok::<_, anyhow::Error>(capacity);
                            }
                        }
                    })
                    .await
                    .context("HTTP/2 output flow control stalled")??;
                    let chunk = data.split_to(capacity.min(data.len()));
                    output.send_data(chunk, false)?;
                }
                output.reserve_capacity(0);
            }
            Err(frame) => {
                if let Ok(mut trailers) = frame.into_trailers() {
                    compression::never_index(&mut trailers);
                    anyhow::ensure!(
                        headers.reserve(id, &trailers),
                        "HTTP/2 trailer budget exhausted"
                    );
                    if let Err(error) = output.send_trailers(trailers) {
                        headers.unsent(id);
                        return Err(error.into());
                    }
                    return Ok(());
                }
            }
        }
    }
}
