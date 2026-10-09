//! Bounded JSON/SSE serialization. One producer per admitted HTTP consumer.

use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::response::Response;
use http::{HeaderValue, header};
use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, mpsc, watch};

use super::super::output::{self, Budget, Lease, Scope};
use super::Progress;
use super::projection::{Agent, Arguments, Initial, Item, Projection, Text};

const CHUNK_BYTES: usize = 16 * 1024;

type Chunk = io::Result<Bytes>;

struct Writer {
    scope: Arc<Scope>,
    sender: mpsc::Sender<Chunk>,
    consumer: Arc<OwnedSemaphorePermit>,
    runtime: tokio::runtime::Handle,
    buffer: Vec<u8>,
    lease: Option<Lease>,
    sequence: u64,
}

struct ConsumerBytes {
    bytes: Bytes,
    _consumer: Arc<OwnedSemaphorePermit>,
}
impl AsRef<[u8]> for ConsumerBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Writer {
    fn chunk(&mut self) -> io::Result<()> {
        if let Some(lease) = self.lease.take() {
            let bytes = output::retain(std::mem::take(&mut self.buffer), lease);
            let bytes = Bytes::from_owner(ConsumerBytes {
                bytes,
                _consumer: self.consumer.clone(),
            });
            self.sender.blocking_send(Ok(bytes)).map_err(|_| closed())?;
        }
        Ok(())
    }

    fn event(&mut self, kind: &str, payload: &impl Serialize) -> io::Result<()> {
        #[derive(Serialize)]
        struct Event<'a, T: Serialize> {
            #[serde(rename = "type")]
            kind: &'a str,
            sequence_number: u64,
            #[serde(flatten)]
            payload: &'a T,
        }
        if kind.contains(['\n', '\r']) {
            return Err(io::Error::other("invalid SSE event name"));
        }
        // SSE framing permits a logical event to span arbitrarily many HTTP
        // chunks; JSON escaping keeps all data on one protocol line.
        // <https://html.spec.whatwg.org/multipage/server-sent-events.html#event-stream-interpretation>
        write!(self, "event: {kind}\ndata: ")?;
        let sequence_number = self.sequence;
        serde_json::to_writer(
            &mut *self,
            &Event {
                kind,
                sequence_number,
                payload,
            },
        )?;
        self.write_all(b"\n\n")?;
        self.sequence += 1;
        Ok(())
    }

    fn changed(&mut self, progress: &mut watch::Receiver<Progress>) -> io::Result<()> {
        let changed = self.runtime.block_on(async {
            tokio::select! {
                _ = self.sender.closed() => Err(closed()),
                result = progress.changed() => result.map(|()| true).map_err(|_| closed()),
                _ = tokio::time::sleep(Duration::from_secs(15)) => Ok(false),
            }
        })?;
        if !changed {
            self.write_all(b":\n\n")?;
            self.flush()?;
        }
        Ok(())
    }

    fn failure(
        &mut self,
        error: bitrouter_orchestrator::core::protocol::CoreError,
    ) -> io::Result<()> {
        #[derive(Serialize)]
        struct Failure {
            error: bitrouter_orchestrator::core::protocol::CoreError,
        }
        self.event("error", &Failure { error })
    }
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.sender.is_closed() {
            return Err(closed());
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.lease.is_none() {
            let lease = self.runtime.block_on(async {
                tokio::select! {
                    _ = self.sender.closed() => Err(closed()),
                    lease = self.scope.reserve(CHUNK_BYTES, false) => lease.map_err(io::Error::other),
                }
            })?;
            self.buffer = Vec::with_capacity(lease.len());
            self.lease = Some(lease);
        }
        let capacity = self.lease.as_ref().map_or(0, Lease::len);
        let take = bytes.len().min(capacity - self.buffer.len());
        self.buffer.extend_from_slice(&bytes[..take]);
        if self.buffer.len() == capacity {
            self.chunk()?;
        }
        Ok(take)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.chunk()
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "managed HTTP consumer closed")
}

fn body(
    scope: Arc<Scope>,
    permit: OwnedSemaphorePermit,
    producer: impl FnOnce(&mut Writer) -> io::Result<()> + Send + 'static,
) -> Body {
    let consumer = Arc::new(permit);
    let (sender, receiver) = mpsc::channel(1);
    let mut writer = Writer {
        scope,
        sender: sender.clone(),
        consumer: consumer.clone(),
        runtime: tokio::runtime::Handle::current(),
        buffer: Vec::new(),
        lease: None,
        sequence: 0,
    };
    // serde is synchronous. At most the host's admitted consumer count can
    // occupy blocking threads; backpressure and receiver loss bound every wait.
    // <https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html>
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            producer(&mut writer)?;
            writer.flush()
        })
        .await;
        let error = match result {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(error) => Some(io::Error::other(error)),
        };
        if let Some(error) = error {
            let _ = sender.send(Err(error)).await;
        }
    });
    Body::from_stream(futures::stream::unfold(
        (receiver, consumer),
        |(mut receiver, consumer)| async move {
            receiver
                .recv()
                .await
                .map(|chunk| (chunk, (receiver, consumer)))
        },
    ))
}

pub(super) fn json(
    exchange: Arc<bitrouter_orchestrator::core::session::responses::ResponseExchange>,
    scope: Arc<Scope>,
    permit: OwnedSemaphorePermit,
) -> Response {
    let mut response = Response::new(body(scope, permit, move |writer| {
        let projection = Projection::new(&exchange).map_err(|error| io::Error::other(error.0))?;
        serde_json::to_writer(writer, &projection).map_err(io::Error::other)
    }));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Serialize)]
struct ResponsePayload<'a, T: Serialize> {
    response: &'a T,
}
#[derive(Serialize)]
struct ItemPayload<'a> {
    output_index: usize,
    item: &'a Item<'a>,
    agent: Agent<'a>,
}
#[derive(Serialize)]
struct PartPayload<'a> {
    item_id: &'a str,
    output_index: usize,
    content_index: usize,
    part: Text<'a>,
    agent: Agent<'a>,
}
#[derive(Serialize)]
struct TextPayload<'a> {
    item_id: &'a str,
    output_index: usize,
    content_index: usize,
    agent: Agent<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
}
#[derive(Serialize)]
struct ArgumentsPayload<'a> {
    item_id: &'a str,
    output_index: usize,
    agent: Agent<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<Arguments<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<Arguments<'a>>,
}

fn item_events(writer: &mut Writer, index: usize, item: &Item<'_>) -> io::Result<()> {
    writer.event(
        "response.output_item.added",
        &ItemPayload {
            output_index: index,
            item: &item.added(),
            agent: item.agent,
        },
    )?;
    if let Some(text) = item.content.and_then(|parts| parts.0) {
        writer.event(
            "response.content_part.added",
            &PartPayload {
                item_id: &item.id,
                output_index: index,
                content_index: 0,
                part: Text::new(""),
                agent: item.agent,
            },
        )?;
        writer.event(
            "response.output_text.delta",
            &TextPayload {
                item_id: &item.id,
                output_index: index,
                content_index: 0,
                delta: Some(text.text),
                text: None,
                agent: item.agent,
            },
        )?;
        writer.event(
            "response.output_text.done",
            &TextPayload {
                item_id: &item.id,
                output_index: index,
                content_index: 0,
                delta: None,
                text: Some(text.text),
                agent: item.agent,
            },
        )?;
        writer.event(
            "response.content_part.done",
            &PartPayload {
                item_id: &item.id,
                output_index: index,
                content_index: 0,
                part: text,
                agent: item.agent,
            },
        )?;
    } else if item.kind == "function_call" {
        writer.event(
            "response.function_call_arguments.delta",
            &ArgumentsPayload {
                item_id: &item.id,
                output_index: index,
                delta: item.arguments,
                arguments: None,
                agent: item.agent,
            },
        )?;
        writer.event(
            "response.function_call_arguments.done",
            &ArgumentsPayload {
                item_id: &item.id,
                output_index: index,
                delta: None,
                arguments: item.arguments,
                agent: item.agent,
            },
        )?;
    }
    writer.event(
        "response.output_item.done",
        &ItemPayload {
            output_index: index,
            item,
            agent: item.agent,
        },
    )
}

pub(super) fn sse(
    mut progress: watch::Receiver<Progress>,
    budget: Arc<Budget>,
    session_limit: u64,
    scope: Arc<Scope>,
    permit: OwnedSemaphorePermit,
) -> Response {
    let mut response = Response::new(body(scope, permit, move |writer| {
        let mut created = false;
        loop {
            let current = progress.borrow_and_update().clone();
            if !created && let Some(initial) = current.initial {
                writer.scope = match budget.scope(session_limit, Some(initial.run(session_limit))) {
                    Ok(scope) => scope,
                    Err(error) => return writer.failure(error),
                };
                writer.event(
                    "response.created",
                    &ResponsePayload::<Initial> { response: &initial },
                )?;
                writer.event(
                    "response.in_progress",
                    &ResponsePayload::<Initial> { response: &initial },
                )?;
                writer.flush()?;
                created = true;
            }
            if let Some(outcome) = current.outcome {
                match outcome {
                    Ok(exchange) => {
                        let projection = match Projection::new(&exchange) {
                            Ok(projection) => projection,
                            Err(error) => return writer.failure(error.0),
                        };
                        for (index, item) in projection.output.iter().enumerate() {
                            item_events(
                                writer,
                                index,
                                &item.map_err(|error| io::Error::other(error.0))?,
                            )?;
                        }
                        for retained in &exchange.events {
                            #[derive(Serialize)]
                            struct Retained<'a> {
                                agent: Agent<'a>,
                                event: &'a bitrouter_orchestrator::core::checkpoint::DurableEvent,
                            }
                            writer.event(
                                &format!("bitrouter.{}", retained.event.kind),
                                &Retained {
                                    agent: Agent {
                                        agent_name: &retained.agent_name,
                                    },
                                    event: &retained.event,
                                },
                            )?;
                        }
                        writer.event(
                            if projection.status == "completed" {
                                "response.completed"
                            } else {
                                "response.failed"
                            },
                            &ResponsePayload {
                                response: &projection,
                            },
                        )?;
                    }
                    Err(error) => writer.failure(error)?,
                }
                return Ok(());
            }
            writer.changed(&mut progress)?;
        }
    }));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{FutureExt, StreamExt};
    use tokio::sync::{Semaphore, oneshot};

    #[tokio::test]
    async fn consumer_capacity_follows_buffered_chunks_after_body_drop()
    -> Result<(), Box<dyn std::error::Error>> {
        let budget = Arc::new(Budget::default());
        let scope = budget.scope(3, None)?;
        let capacity = Arc::new(Semaphore::new(1));
        let permit = capacity.clone().acquire_owned().await?;
        let mut stream =
            body(scope.clone(), permit, |writer| writer.write_all(b"oneend")).into_data_stream();
        let first = stream.next().await.ok_or("first chunk")??;
        let retained = first.clone();
        drop(first);
        assert!(stream.next().now_or_never().is_none());
        drop(retained);
        let last = stream.next().await.ok_or("last chunk")??;
        assert!(stream.next().await.is_none());
        drop(stream);
        assert_eq!(capacity.available_permits(), 0);
        assert!(scope.reserve(1, false).now_or_never().is_none());
        let retained = last.clone();
        drop(last);
        assert_eq!(capacity.available_permits(), 0);
        drop(retained);
        assert_eq!(capacity.available_permits(), 1);
        assert_eq!(scope.reserve(3, true).await?.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn closing_a_consumer_interrupts_its_shared_byte_wait()
    -> Result<(), Box<dyn std::error::Error>> {
        let budget = Arc::new(Budget::default());
        let scope = budget.scope(64, None)?;
        let capacity = Arc::new(Semaphore::new(2));
        let mut first = body(
            scope.clone(),
            capacity.clone().acquire_owned().await?,
            |writer| writer.write_all(&[1; 128]),
        )
        .into_data_stream();
        let chunk = first.next().await.ok_or("retained chunk")??;
        let (started, start) = oneshot::channel();
        let (finished, finish) = oneshot::channel();
        let second = body(
            scope.clone(),
            capacity.clone().acquire_owned().await?,
            move |writer| {
                let _ = started.send(());
                let result = writer.write_all(&[2; 128]);
                let _ = finished.send(());
                result
            },
        );
        start.await?;
        drop(second);
        tokio::time::timeout(Duration::from_secs(5), finish).await??;
        // The first consumer still owns all session bytes, yet the closed
        // second producer has stopped instead of waiting for that consumer.
        assert!(scope.reserve(1, false).now_or_never().is_none());
        drop(first);
        drop(chunk);
        let _permits =
            tokio::time::timeout(Duration::from_secs(5), capacity.acquire_many(2)).await??;
        Ok(())
    }

    #[tokio::test]
    async fn json_and_nested_arguments_stream_through_a_tiny_budget()
    -> Result<(), Box<dyn std::error::Error>> {
        let value = serde_json::json!({"path": "\\\"\n\u{0000}🦀".repeat(5000), "number": 1});
        let expected = value.to_string();
        let budget = Arc::new(Budget::default());
        let scope = budget.scope(17, None)?;
        let capacity = Arc::new(Semaphore::new(1));
        let mut stream = body(
            scope.clone(),
            capacity.clone().acquire_owned().await?,
            move |writer| {
                #[derive(Serialize)]
                struct View<'a> {
                    arguments: Arguments<'a>,
                }
                serde_json::to_writer(
                    writer,
                    &View {
                        arguments: Arguments::Json(&value),
                    },
                )
                .map_err(io::Error::other)
            },
        )
        .into_data_stream();
        let mut received = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            assert!(chunk.len() <= 17);
            received.extend_from_slice(&chunk);
        }
        drop(stream);
        let decoded: serde_json::Value = serde_json::from_slice(&received)?;
        assert_eq!(decoded["arguments"], expected);
        assert_eq!(scope.reserve(17, true).await?.len(), 17);
        assert_eq!(capacity.available_permits(), 1);
        Ok(())
    }
    #[tokio::test]
    async fn unsupported_retained_output_ends_with_a_typed_sse_error()
    -> Result<(), Box<dyn std::error::Error>> {
        use bitrouter_ai::types::{Content, DataContent, Message, Role};
        use bitrouter_orchestrator::core::session::responses::{ResponseExchange, ResponseOutput};
        let mut exchange: ResponseExchange = serde_json::from_value(serde_json::json!({
            "response_id":"response", "operation_id":"operation", "run_id":"run", "created_at":1,
            "previous_response_id":null, "created_state_revision":1, "completed_state_revision":2,
            "run_status":"completed", "output":[], "pending":{}
        }))?;
        exchange.output.push(ResponseOutput {
            event_seq: 1,
            agent_id: "root".into(),
            agent_name: "/root".into(),
            agent_turn_id: "turn".into(),
            step_id: "step".into(),
            message: Message {
                role: Role::Assistant,
                content: vec![Content::File {
                    media_type: "image/png".into(),
                    data: DataContent::Base64 {
                        data: "AQ==".into(),
                    },
                    filename: None,
                    provider_metadata: Default::default(),
                }],
            },
            call_ids: Default::default(),
        });
        let initial = Arc::new(Initial::new(&exchange).map_err(|error| error.0)?);
        let (_sender, progress) = watch::channel(Progress {
            initial: Some(initial),
            outcome: Some(Ok(Arc::new(exchange))),
        });
        let budget = Arc::new(Budget::default());
        let scope = budget.scope(128, None)?;
        let permit = Arc::new(Semaphore::new(1)).acquire_owned().await?;
        let mut stream = sse(progress, budget, 128, scope, permit)
            .into_body()
            .into_data_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk?);
        }
        let text = String::from_utf8(bytes)?;
        let events = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["type"], "response.created");
        assert_eq!(events[1]["type"], "response.in_progress");
        assert_eq!(events[2]["type"], "error");
        assert_eq!(events[2]["sequence_number"], 2);
        assert_eq!(events[2]["error"]["code"], "unsupported_capability");
        Ok(())
    }
}
