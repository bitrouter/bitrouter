//! Ordinary HTTP clients retain byte/EOF evidence; a timeout is a test failure.

use std::collections::BTreeMap;

use tokio::sync::oneshot;

use super::*;

#[derive(Default)]
pub(super) struct Clients {
    tasks: JoinSet<(String, Result<Trace>)>,
    completed: BTreeMap<String, Trace>,
}

pub(super) struct Trace {
    pub(super) head: Option<Wire>,
    pub(super) bytes: Vec<u8>,
    pub(super) transport_error: bool,
    pub(super) completed_at: Instant,
}

#[derive(Default)]
pub(super) struct Frames {
    pub(super) held: bool,
    pub(super) terminal: bool,
    pub(super) done: bool,
    pub(super) error: bool,
}

impl Clients {
    pub(super) fn start(
        &mut self,
        label: &str,
        request: reqwest::RequestBuilder,
    ) -> oneshot::Receiver<Wire> {
        let label = label.to_owned();
        let (first, started) = oneshot::channel();
        self.tasks
            .spawn(async move { (label, record(request, first).await) });
        started
    }

    pub(super) async fn finished(&mut self, label: &str) -> Result<Trace> {
        tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                if let Some(trace) = self.completed.remove(label) {
                    return Ok(trace);
                }
                let (actual, trace) = self
                    .tasks
                    .join_next()
                    .await
                    .context("missing client task")??;
                let trace = trace.with_context(|| format!("client {actual}"))?;
                ensure!(
                    self.completed.insert(actual, trace).is_none(),
                    "duplicate client label"
                );
            }
        })
        .await
        .context("client did not finish after observed shutdown/release")?
    }

    pub(super) fn consumed(&self) -> Result<()> {
        ensure!(
            self.tasks.is_empty() && self.completed.is_empty(),
            "unconsumed client evidence"
        );
        Ok(())
    }

    pub(super) async fn finish(mut self) -> Result<()> {
        self.tasks.abort_all();
        let mut cleanup = Vec::new();
        while let Some(joined) = self.tasks.join_next().await {
            match joined {
                Ok((label, result)) => cleanup.push(
                    result
                        .map(|_| ())
                        .with_context(|| format!("joining {label}")),
                ),
                Err(error) if error.is_cancelled() => {}
                Err(error) => cleanup.push(Err(error).context("joining client task")),
            }
        }
        combine_results(Ok(()), cleanup)
    }
}

async fn record(request: reqwest::RequestBuilder, first: oneshot::Sender<Wire>) -> Result<Trace> {
    let mut response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            ensure!(
                !error.is_timeout(),
                "client header timeout is not interruption evidence: {error}"
            );
            return Ok(Trace {
                head: None,
                bytes: Vec::new(),
                transport_error: true,
                completed_at: Instant::now(),
            });
        }
    };
    let head = Wire {
        status: response.status(),
        headers: response.headers().clone(),
        body: Vec::new(),
    };
    let stream = head
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let mut trace = Trace {
        head: Some(head),
        bytes: Vec::new(),
        transport_error: false,
        completed_at: Instant::now(),
    };
    let mut first = Some(first);
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                ensure!(
                    trace.bytes.len().saturating_add(chunk.len()) <= 64 * 1024,
                    "client evidence exceeded bound"
                );
                trace.bytes.extend_from_slice(&chunk);
                if stream
                    && frames(&trace.bytes)?.held
                    && let Some(first) = first.take()
                {
                    let head = trace.head.as_ref().context("missing stream headers")?;
                    let _ = first.send(Wire {
                        status: head.status,
                        headers: head.headers.clone(),
                        body: Vec::new(),
                    });
                }
            }
            Ok(None) => {
                trace.completed_at = Instant::now();
                return Ok(trace);
            }
            Err(error) => {
                ensure!(
                    !error.is_timeout(),
                    "client body timeout is not interruption evidence: {error}"
                );
                trace.transport_error = true;
                trace.completed_at = Instant::now();
                return Ok(trace);
            }
        }
    }
}

pub(super) fn frames(bytes: &[u8]) -> Result<Frames> {
    let mut observed = Frames::default();
    // Inspect complete SSE frames only; a split/truncated final frame cannot be
    // promoted into a completed event. Test payloads are deliberately ASCII.
    let text = std::str::from_utf8(bytes)?.replace("\r\n", "\n");
    for frame in text
        .split_inclusive("\n\n")
        .filter(|frame| frame.ends_with("\n\n"))
    {
        let data = frame
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            observed.done = true;
            continue;
        }
        let value: Value = serde_json::from_str(&data)?;
        observed.error |= value.get("error").is_some_and(Value::is_object);
        if let Some(choices) = value["choices"].as_array() {
            for choice in choices {
                observed.held |= choice["delta"]["content"].as_str() == Some("held");
                observed.terminal |= choice
                    .get("finish_reason")
                    .is_some_and(|reason| !reason.is_null());
            }
        }
    }
    Ok(observed)
}

impl Trace {
    pub(super) fn success(&self, id: &str, tag: &str, streaming: bool) -> Result<()> {
        let head = self.head.as_ref().context("success had no HTTP headers")?;
        head.expect_status(StatusCode::OK)?;
        head.identity(id, tag)?;
        ensure!(
            !self.transport_error,
            "successful response ended with transport error"
        );
        if streaming {
            let observed = frames(&self.bytes)?;
            ensure!(
                observed.held && observed.terminal && observed.done && !observed.error,
                "incomplete successful stream"
            );
        } else {
            let body: Value = serde_json::from_slice(&self.bytes)?;
            ensure!(body["choices"][0]["finish_reason"] == "stop");
        }
        Ok(())
    }

    pub(super) fn interrupted(&self, id: &str, tag: &str, streaming: bool) -> Result<()> {
        if streaming {
            let head = self.head.as_ref().context("stream never started")?;
            head.expect_status(StatusCode::OK)?;
            head.identity(id, tag)?;
            let observed = frames(&self.bytes)?;
            ensure!(
                observed.held && !observed.terminal && (!observed.done || observed.error),
                "interruption claimed a successful stream terminal"
            );
            // The stock Chat renderer may end an explicit error frame with
            // [DONE]. That closes an error response; it is not a success signal.
            // finished() observed a socket error or EOF. EOF without a protocol
            // terminal is a truncated stream, never successful client delivery.
        } else if let Some(head) = &self.head {
            ensure!(
                !head.status.is_success(),
                "uncompleted provider call returned success"
            );
        } else {
            ensure!(
                self.transport_error,
                "missing response without transport evidence"
            );
        }
        Ok(())
    }
}

#[test]
fn incomplete_sse_frame_never_establishes_a_terminal() -> Result<()> {
    let partial = b"data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n";
    ensure!(!frames(partial)?.terminal);
    let mut complete = partial.to_vec();
    complete.push(b'\n');
    ensure!(frames(&complete)?.terminal);
    Ok(())
}

#[test]
fn explicit_error_then_done_is_interruption_but_bare_done_is_not() -> Result<()> {
    let held = "data: {\"choices\":[{\"delta\":{\"content\":\"held\"}}]}\n\n";
    let error = "data: {\"error\":{\"type\":\"upstream_error\",\"message\":\"interrupted\"}}\n\n";
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-bitrouter-execution-id", "id".parse()?);
    headers.insert("x-bitrouter-request-id", "tag".parse()?);
    headers.insert("location", "/v1/gateway/executions/id".parse()?);
    let mut trace = Trace {
        head: Some(Wire {
            status: StatusCode::OK,
            headers,
            body: Vec::new(),
        }),
        bytes: format!("{held}{error}data: [DONE]\n\n").into_bytes(),
        transport_error: false,
        completed_at: Instant::now(),
    };
    trace.interrupted("id", "tag", true)?;
    ensure!(trace.success("id", "tag", true).is_err());
    trace.bytes = format!("{held}data: [DONE]\n\n").into_bytes();
    ensure!(trace.interrupted("id", "tag", true).is_err());
    Ok(())
}
