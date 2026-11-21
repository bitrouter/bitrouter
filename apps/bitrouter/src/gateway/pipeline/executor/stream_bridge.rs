//! Complete Responses output and actual request provenance for the native SSE bridge.
//! <https://developers.openai.com/api/docs/guides/streaming-responses>

use super::*;

#[derive(Default)]
struct Source {
    terminal: Option<serde_json::Value>,
    reasoning_streamed: bool,
    opened: std::collections::BTreeSet<u64>,
    done: std::collections::BTreeMap<u64, serde_json::Value>,
    scrubber: Option<UpstreamErrorScrubber>,
}

#[derive(Default)]
pub(super) struct BridgeCapture(Mutex<Source>);

impl BridgeCapture {
    fn source(&self) -> std::sync::MutexGuard<'_, Source> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn terminal(&self, event: &SseEvent) -> Result<()> {
        let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&event.data) else {
            return Ok(());
        };
        let event_type = json
            .get("type")
            .and_then(serde_json::Value::as_str)
            .or(event.event.as_deref());
        let mut source = self.source();
        if matches!(
            event_type,
            Some("response.reasoning_summary_text.delta" | "response.reasoning_text.delta")
        ) || (matches!(
            event_type,
            Some("response.output_item.added" | "response.output_item.done")
        ) && json
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(serde_json::Value::as_str)
            == Some("reasoning"))
        {
            source.reasoning_streamed = true;
        }
        match event_type {
            Some("response.output_item.added") => {
                if let Some(index) = json.get("output_index").and_then(serde_json::Value::as_u64) {
                    source.opened.insert(index);
                }
            }
            Some("response.output_item.done") => {
                if let (Some(index), Some(item)) = (
                    json.get("output_index").and_then(serde_json::Value::as_u64),
                    json.get("item"),
                ) {
                    source.done.insert(index, item.clone());
                }
            }
            Some("response.completed" | "response.incomplete") => {
                if let Some(mut response) = json.get_mut("response").map(serde_json::Value::take) {
                    // Codex's Responses-Lite stream sends complete items in .done
                    // frames and an empty terminal output. Preserve those final
                    // items, including calls and private reasoning, never deltas.
                    // https://github.com/openai/codex/blob/main/codex-rs/codex-api/src/sse/responses.rs
                    if response
                        .get("output")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(Vec::is_empty)
                        && (!source.opened.is_empty() || !source.done.is_empty())
                    {
                        let contiguous =
                            source.done.keys().copied().eq(0..source.done.len() as u64);
                        if !contiguous
                            || source
                                .opened
                                .iter()
                                .any(|index| !source.done.contains_key(index))
                        {
                            return Err(BitrouterError::UpstreamInvalidResponse {
                                message: "Codex Lite terminal has unfinished output items".into(),
                                usage: crate::gateway::model::protocol::OutboundAdapter::parse_response(
                                    &crate::gateway::model::protocol::responses::ResponsesAdapter,
                                    response.clone(),
                                )
                                .ok()
                                .and_then(|result| result.usage)
                                .map(Box::new),
                            });
                        }
                        response["output"] = serde_json::Value::Array(
                            std::mem::take(&mut source.done).into_values().collect(),
                        );
                    }
                    source.terminal = Some(response);
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn complete(
        &self,
        executor: &HttpExecutor,
        target: &RoutingTarget,
        mut folded: GenerateResult,
    ) -> Result<GenerateResult> {
        if target.api_protocol != ApiProtocol::Responses {
            return Ok(folded);
        }
        let mut source = self.source();
        let Some(terminal) = source.terminal.take() else {
            return Ok(folded);
        };
        validate_nonstream_responses_terminal(&terminal)?;
        // The terminal response carries the full output, including private reasoning
        // items and provider tools omitted from the delta IR. Do not concatenate it
        // with deltas or certify the lossy delta projection as complete history.
        // https://developers.openai.com/api/reference/resources/responses/streaming-events
        if !terminal
            .get("output")
            .is_some_and(serde_json::Value::is_array)
        {
            // An incomplete terminal cannot bind the delta projection to its
            // provider state. Keep received usage, but certify neither its
            // private origin nor a continuation prefix.

            return Ok(folded);
        }
        let (adapter, _) = executor
            .dispatch
            .lookup(&target.api_protocol)
            .ok_or_else(|| HttpExecutor::no_dispatch_error(target))?;
        folded =
            parse_upstream_success(adapter.as_ref(), terminal).map_err(|error| {
                match &source.scrubber {
                    Some(scrubber) => scrubber.scrub_error(error),
                    None => error,
                }
            })?;

        Ok(folded)
    }
}

impl HttpExecutor {
    pub(super) async fn execute_http_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
        capture: Option<Arc<BridgeCapture>>,
    ) -> Result<StreamPartStream> {
        let (adapter, transport) = self
            .dispatch
            .lookup(&target.api_protocol)
            .ok_or_else(|| Self::no_dispatch_error(target))?;

        Self::check_response_format(prompt, adapter, target)?;

        let mut error_scrubber = UpstreamErrorScrubber::new(None);
        error_scrubber.capture_target(target, ctx);
        error_scrubber.redactor.capture_prompt_continuity(prompt);
        let url = transport.endpoint_url(&target.model_target(), true);
        let trace_headers = ctx.take_outbound_trace_headers();

        let (client, _) = self.client_for(
            target,
            super::shared::has_admission(ctx) || super::tenancy::has_execution(ctx),
        );

        let mut http_source = if target.api_protocol == ApiProtocol::Responses {
            HttpSource::new(prompt, ctx)
        } else {
            None
        };
        let mut attempted_auth_refresh = false;
        let (response, mut shared_attempt, mut tenant_attempt) = loop {
            let mut body =
                self.render_execution_request(adapter.as_ref(), target, prompt, ctx, true)?;

            if self.needs_body_shaping(target, ctx)? {
                self.shape_request_body(&mut body, target, ctx)
                    .await
                    .map_err(|error| error_scrubber.scrub_error(error))?;
            }
            let continuation_substitution = apply_provider_continuation(&mut body, target, ctx)?;
            if let Some(continuation) = continuation_substitution {
                error_scrubber
                    .redactor
                    .add_replacement(continuation.native, continuation.public_or_redacted);
            }
            let request_input = RequestBuildInput {
                client: &client,

                url: &url,
                body: &body,
                target,
                transport,
                ctx,
                trace_headers: trace_headers.as_ref(),
            };

            let applied = self
                .build_authenticated_request(&request_input)
                .await
                .map_err(|error| error_scrubber.scrub_error(error))?;
            let mut shared_attempt =
                super::shared::Attempt::begin(self.shared_authority, &applied, target, ctx).await?;
            let tenant_binding = applied.tenant_binding();
            let (mut request, authority, mut tenant_attempt) = applied.into_parts();
            if let Some(source) = &mut http_source {
                source.authenticated(&request, authority, prompt);
            }

            error_scrubber.capture_request_credentials(&request, target);
            let rejected_authorization = request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .cloned();
            let mut native_attempt = None;
            let mut private_adapter = false;
            let response = async {
                let routing_guard = if super::tenancy::legacy_routing(ctx) {
                    Some(crate::tenancy::routing::admit(ctx, target)?)
                } else {
                    None
                };
                record_native_dispatch(prompt, target, ctx)?;
                let admission = super::tenancy::admit_send(
                    tenant_binding.as_ref(),
                    &mut tenant_attempt,
                    &mut request,
                    target,
                    ctx,
                    Some(true),
                )?;
                if let Some(guard) = routing_guard {
                    guard.sent()?;
                }
                let response = admission
                    .send(&client, request, ctx, &mut error_scrubber)
                    .await;
                private_adapter = admission.adapter.is_some();
                native_attempt = admission.attempt_id;
                response
            }
            .await?;

            let status = response.status();
            receipt::status(
                &mut tenant_attempt,
                status.as_u16(),
                parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER)),
            )?;
            if !status.is_success() {
                crate::tenancy::routing::response_failed(
                    ctx,
                    target,
                    &classify_upstream_error(
                        status.as_u16(),
                        "",
                        parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER)),
                    ),
                );
            }
            let retry_after =
                parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER));
            if status.is_success() {
                if let Some(source) = &mut http_source {
                    source.binding = tenant_binding.clone();
                    source.attempt = native_attempt.clone();
                }
                if let Some(capture) = &capture {
                    let mut source = capture.source();
                    source.scrubber = Some(error_scrubber.clone());
                }
                break (response, shared_attempt, tenant_attempt);
            }
            shared_attempt.rejected(status.as_u16()).await?;
            let text = response_body::read(response, ctx)
                .await
                .map_err(|error| error_scrubber.scrub_error(error))?;
            receipt::error_json_text(&mut tenant_attempt, &target.api_protocol, &text)?;
            if status == reqwest::StatusCode::UNAUTHORIZED
                && !private_adapter
                && !attempted_auth_refresh
                && self
                    .refresh_auth_after_unauthorized(target, rejected_authorization.as_ref(), ctx)
                    .await
                    .map_err(|error| error_scrubber.scrub_error(error))?
            {
                attempted_auth_refresh = true;
                continue;
            }
            let scrubbed = error_scrubber.scrub_body(&text);
            return Err(classify_upstream_error(
                status.as_u16(),
                &scrubbed,
                retry_after,
            ));
        };

        // Parse the upstream SSE byte stream into canonical stream parts via
        // the protocol's stateful decoder.
        let mut decoder = adapter.stream_decoder();
        let protocol = adapter.protocol();
        let limit = response_body::limit(ctx);
        let cancellation = super::tenancy::cancellation(ctx)?;
        response_body::check_length(&response, limit)?;
        let byte_stream = response_body::bounded(response.bytes_stream(), limit);
        let operational = tenant_attempt
            .as_ref()
            .and_then(|permit| permit.operational())
            .cloned();
        #[cfg(feature = "server-tools")]
        let mut tool_turn = crate::tenancy::routing::tool_stream_turn(ctx, target);

        let stream = async_stream::stream! {
            use eventsource_stream::Eventsource;
            let mut terminal = false;
            // Shared delivery cannot expose a successful terminal before its
            // accounting fact commits. Retain only this one bounded part while
            // accepting a provider's trailing usage report and validating EOF.
            let mut held_terminal = None;
            let events = byte_stream.eventsource();
            futures::pin_mut!(events);
            loop {
                let event = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {
                        drop(tenant_attempt.take());
                        yield Err(BitrouterError::UpstreamUnavailable);
                        return;
                    }
                    event = events.next() => event,
                };
                let Some(event) = event else { break };
                match event {
                    Ok(ev) => {
                        let sse = SseEvent {
                            event: if ev.event.is_empty() { None } else { Some(ev.event) },
                            data: ev.data,
                        };
                        match decoder.decode(&sse) {
                            Ok(parts) => {
                                if let Err(error) = receipt::batch(&tenant_attempt, &parts, held_terminal.is_some()).and_then(|()| receipt::event(&mut tenant_attempt, &protocol, &sse)) { drop(tenant_attempt.take()); yield Err(error); return; }
                                if let Err(error) = shared_attempt.observe_batch(&parts, held_terminal.is_some()) {
                                    drop(tenant_attempt.take()); yield Err(error);
                                    return;
                                }
                                if let Some(part) = decoder.pending_terminal() {
                                    shared_attempt.observe(&part);
                                    // The decoder accepted these counters even
                                    // while completion awaits transport EOF.
                                    if let StreamPart::ResponseCompleted { usage: Some(usage), .. } = part {
                                        yield Ok(StreamPart::Usage { usage });
                                    }
                                }
                                if let Some(capture) = &capture
                                    && let Err(error) = capture.terminal(&sse) {
                                    drop(tenant_attempt.take()); yield Err(error_scrubber.scrub_error(error));
                                    return;
                                }
                                if let Some(source) = &mut http_source
                                    && let Err(error) = source.terminal(&sse)
                                {
                                    drop(tenant_attempt.take());
                                    yield Err(error_scrubber.scrub_error(error));
                                    return;
                                }
                                for p in parts {
                                    #[cfg(feature = "server-tools")]
                                    if let Some(turn) = &mut tool_turn { turn.observe(&p); }
                                    if let Some(source) = &mut http_source { source.delivered.observe(&p); }
                                    terminal |= p.is_terminal();
                                    if (shared_attempt.is_shared() || tenant_attempt.is_some() || http_source.is_some()) && p.is_terminal() {
                                        // Received usage has independent custody from
                                        // the EOF/ACK-gated successful terminal.
                                        if let StreamPart::ResponseCompleted { usage: Some(usage), .. } = &p {
                                            yield Ok(StreamPart::Usage { usage: usage.clone() });
                                        }
                                        held_terminal = Some(p); continue;
                                    }
                                    yield Ok(p);
                                }
                            }
                            Err(e) => {
                                // Retain valid counts from this same event before
                                // persisting its failure or yielding to a caller.
                                let reported = error_event_usage(&protocol, &sse, &e);
                                if let Some(usage) = &reported {
                                    shared_attempt.observe(&StreamPart::Usage { usage: usage.clone() });
                                    // The same qualified source observation must
                                    // reach tenant evidence before its canonical
                                    // projection. Invalid content still leaves
                                    // the attempt unresolved; usage is no terminal.
                                    if let Err(error) = receipt::event(&mut tenant_attempt, &protocol, &sse) {
                                        drop(tenant_attempt.take()); yield Err(error);
                                        return;
                                    }
                                }
                                if matches!(&e, crate::gateway::model::error::ModelError::Provider { .. } | crate::gateway::model::error::ModelError::PolicyViolation { .. })
                                    && let Err(error) = shared_attempt.failed_stream().await
                                {
                                    drop(tenant_attempt.take()); yield Err(error);
                                    return;
                                }
                                if matches!(&e, crate::gateway::model::error::ModelError::Provider { .. } | crate::gateway::model::error::ModelError::PolicyViolation { .. })
                                    && let Err(error) = (if reported.is_none() { receipt::event(&mut tenant_attempt, &protocol, &sse) } else { Ok(()) }).and_then(|()| receipt::complete(&mut tenant_attempt, false))
                                {
                                    drop(tenant_attempt.take());
                                    yield Err(error);
                                    return;
                                }
                                let mut error = classify_stream_decoder_error(e);
                                if let Some(reported) = reported {
                                    yield Ok(StreamPart::Usage { usage: reported.clone() });
                                    if let BitrouterError::UpstreamInvalidResponse { usage, .. } = &mut error { *usage = Some(Box::new(reported)); }
                                }
                                drop(tenant_attempt.take()); yield Err(error_scrubber.scrub_error(error));
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        // A read-timeout that fires mid-stream arrives here as a
                        // transport error — recover the reqwest timeout signal so
                        // it maps to UpstreamTimeout (504), not a blanket 502.
                        let error = match e {
                            eventsource_stream::EventStreamError::Transport(error) => error,
                            other => stream_transport_error(false, &other),
                        };
                        if let Some(hop) = &operational {
                            if cancellation.is_cancelled() { hop.dropped(); }
                            else if matches!(error, BitrouterError::UpstreamTimeout | BitrouterError::Upstream { .. }) {
                                hop.transport_failed(matches!(error, BitrouterError::UpstreamTimeout));
                            }
                        }
                        drop(tenant_attempt.take()); yield Err(error_scrubber.scrub_error(error));
                        return;
                    }
                }
            }
            match decoder.finish() {
                Ok(parts) => {
                    if let Err(error) = receipt::batch(&tenant_attempt, &parts, held_terminal.is_some()) { drop(tenant_attempt.take()); yield Err(error); return; }
                    if let Err(error) = shared_attempt.observe_batch(&parts, held_terminal.is_some()) {
                        drop(tenant_attempt.take()); yield Err(error);
                        return;
                    }
                    for p in parts {
                        #[cfg(feature = "server-tools")]
                        if let Some(turn) = &mut tool_turn { turn.observe(&p); }
                        if let Some(source) = &mut http_source { source.delivered.observe(&p); }
                        terminal |= p.is_terminal();
                        if (shared_attempt.is_shared() || tenant_attempt.is_some() || http_source.is_some()) && p.is_terminal() {
                                        // Received usage has independent custody from
                                        // the EOF/ACK-gated successful terminal.
                                        if let StreamPart::ResponseCompleted { usage: Some(usage), .. } = &p {
                                            yield Ok(StreamPart::Usage { usage: usage.clone() });
                                        }
                                        held_terminal = Some(p); continue;
                                    }
                        yield Ok(p);
                    }
                }
                Err(e) => {
                    drop(tenant_attempt.take()); yield Err(error_scrubber.scrub_error(classify_stream_decoder_error(e)));
                    return;
                }
            }
            if !terminal {
                drop(tenant_attempt.take()); yield Err(BitrouterError::UpstreamInvalidResponse { message: "upstream stream ended without a model terminal part".into(), usage: None });
                return;
            }
            if let Some(part) = &held_terminal
                && let Err(error) = receipt::complete(&mut tenant_attempt, receipt::successful(part))
            {
                drop(tenant_attempt.take());
                yield Err(error);
                return;
            }
            if let Err(error) = shared_attempt.finish_stream().await { drop(tenant_attempt.take()); yield Err(error); return; }
            // Receipt completion is evidence, not publication authority. Preserve
            // 034's original terminal/EOF proof and raw replayability before the
            // exact actual-send permit leaves its producer.
            if let Some(source)=http_source {source.publish();}
            drop(tenant_attempt.take());
            #[cfg(feature = "server-tools")]
            if let (Some(turn), Some(part)) = (tool_turn, held_terminal.as_ref()) {
                turn.complete(receipt::successful(part));
            }
            if let Some(part) = held_terminal { yield Ok(part); }
        };

        Ok(Box::pin(
            crate::gateway::providers::google_chat::bind_stream(stream, target.model_target()),
        ))
    }
}

// Only a fixed-size terminal commitment and bounded provider ID survive an
// event. The normal 16MiB incremental ingress bound governs JSON decoding.
struct HttpSource {
    input: super::tenancy::http_continuation::Input,
    binding: Option<super::tenancy::BoundAuthentication>,
    attempt: Option<bitrouter_ai::tenancy::identity::AttemptId>,
    authority: Option<ContinuationAuthority>,
    allowed: bool,
    effort_bound: bool,
    delivered: crate::gateway::model::protocol::responses::StreamingAssistantTurnCommitment,
    terminal: Option<(
        String,
        Option<crate::gateway::model::protocol::responses::AssistantTurnCommitment>,
        bool,
    )>,
}
impl HttpSource {
    fn new(prompt: &Prompt, ctx: &PipelineContext) -> Option<Self> {
        Some(Self {
            input: super::tenancy::http_continuation::Input::new(prompt, ctx)?,
            binding: None,
            attempt: None,
            authority: None,
            allowed: false,
            effort_bound: false,
            delivered: Default::default(),
            terminal: None,
        })
    }
    fn authenticated(
        &mut self,
        request: &reqwest::Request,
        authority: Option<ContinuationAuthority>,
        prompt: &Prompt,
    ) {
        let wire = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .and_then(|body| serde_json::from_slice::<serde_json::Value>(body).ok());
        self.allowed = wire.as_ref().is_some_and(|body| {
            body.get("store") != Some(&serde_json::Value::Bool(false))
                && body.get("stream") == Some(&serde_json::Value::Bool(true))
        });
        self.effort_bound = wire.as_ref().is_some_and(|body| {
            body.get("reasoning")
                .and_then(|r| r.get("effort"))
                .cloned()
                .unwrap_or(serde_json::Value::Null)
                == serde_json::to_value(prompt.params.reasoning_effort)
                    .unwrap_or(serde_json::Value::Null)
        });
        self.authority = authority;
    }
    fn terminal(&mut self, event: &SseEvent) -> Result<()> {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&event.data) else {
            return Ok(());
        };
        if !matches!(
            value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .or(event.event.as_deref()),
            Some("response.completed" | "response.incomplete")
        ) {
            return Ok(());
        }
        let response = value
            .get("response")
            .ok_or_else(|| BitrouterError::bad_request("HTTP continuation terminal missing"))?;
        let id = response
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 512)
            .ok_or_else(|| {
                BitrouterError::bad_request("HTTP continuation terminal identifier invalid")
            })?;
        if self.terminal.is_some() {
            return Err(BitrouterError::bad_request(
                "HTTP continuation terminal repeated",
            ));
        }
        self.terminal = Some((
            id.into(),
            crate::gateway::model::protocol::responses::output_replayable(response)
                .then(|| {
                    crate::gateway::model::protocol::responses::terminal_assistant_turn_commitment(
                        response,
                    )
                })
                .flatten(),
            response.get("store") == Some(&serde_json::Value::Bool(true)),
        ));
        Ok(())
    }
    fn publish(self) {
        if let (Some(binding), Some(attempt), Some(authority), Some((id, turn, stored))) =
            (self.binding, self.attempt, self.authority, self.terminal)
        {
            binding.record_http_source(
                self.input,
                attempt,
                &authority,
                Some(&id),
                super::tenancy::http_continuation::Output {
                    delivered: self.delivered.finish(),
                    reconstructible: turn,
                    stored: stored && self.allowed && self.effort_bound,
                },
            );
        }
    }
}

fn error_event_usage(
    protocol: &ApiProtocol,
    event: &SseEvent,
    error: &crate::gateway::model::error::ModelError,
) -> Option<bitrouter_ai::types::Usage> {
    use crate::gateway::model::error::ModelError;
    use crate::gateway::model::protocol::{chat_completions, responses};
    let body: serde_json::Value = serde_json::from_str(&event.data).ok()?;
    match protocol {
        ApiProtocol::ChatCompletions => body.get("usage").and_then(chat_completions::parse_usage),
        ApiProtocol::Responses
            if matches!(
                error,
                ModelError::Provider { .. } | ModelError::PolicyViolation { .. }
            ) && body
                .get("type")
                .and_then(serde_json::Value::as_str)
                .or(event.event.as_deref())
                == Some("response.failed") =>
        {
            // Only this decoder path has validated failure identity and status.
            // InvalidResponse (including rejected post-terminal data) must not
            // overwrite usage from the response already observed.
            body.get("response")?
                .get("usage")
                .and_then(responses::parse_usage)
        }
        _ => None,
    }
}

#[cfg(test)]
mod lite_tests {
    use super::*;
    use crate::gateway::model::protocol::OutboundAdapter;
    use crate::gateway::model::protocol::responses::ResponsesAdapter;
    use bitrouter_ai::types::Content;

    fn event(value: serde_json::Value) -> SseEvent {
        SseEvent {
            event: None,
            data: value.to_string(),
        }
    }

    #[test]
    fn codex_lite_terminal_retains_complete_calls_and_reasoning() -> Result<()> {
        let capture = BridgeCapture::default();
        let items = [
            serde_json::json!({"type":"reasoning","id":"r1","summary":[],"encrypted_content":"fixture-only"}),
            serde_json::json!({"type":"function_call","id":"f1","call_id":"c1","name":"read","arguments":"{\"path\":\"seed.txt\"}"}),
        ];
        for (index, item) in items.iter().enumerate() {
            capture.terminal(&event(serde_json::json!({"type":"response.output_item.added","output_index":index,"item":item})))?;
            capture.terminal(&event(serde_json::json!({"type":"response.output_item.done","output_index":index,"item":item})))?;
        }
        capture.terminal(&event(serde_json::json!({"type":"response.completed","response":{"id":"resp_lite","status":"completed","output":[]}})))?;
        let terminal = capture
            .source()
            .terminal
            .clone()
            .ok_or_else(|| BitrouterError::internal("missing terminal"))?;
        let result = ResponsesAdapter.parse_response(terminal)?;
        assert!(
            result
                .content
                .iter()
                .any(|part| matches!(part, Content::Reasoning { .. }))
        );
        assert!(result.content.iter().any(|part| matches!(part, Content::ToolCall { name, arguments, .. } if name == "read" && arguments == "{\"path\":\"seed.txt\"}")));
        Ok(())
    }

    #[test]
    fn codex_lite_terminal_rejects_unfinished_items() -> Result<()> {
        let capture = BridgeCapture::default();
        capture.terminal(&event(serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"f1"}})))?;
        assert!(capture.terminal(&event(serde_json::json!({"type":"response.completed","response":{"id":"resp_lite","status":"completed","output":[]}}))).is_err());
        Ok(())
    }
}
