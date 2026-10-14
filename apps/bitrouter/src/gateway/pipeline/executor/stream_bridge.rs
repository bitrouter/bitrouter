//! Complete Responses output and actual request provenance for the native SSE bridge.
//! <https://developers.openai.com/api/docs/guides/streaming-responses>

use super::*;
use bitrouter_ai::types::Content;

#[derive(Default)]
struct Source {
    authority: Option<ContinuationAuthority>,
    storage_allowed: bool,
    effort_bound: bool,
    terminal: Option<serde_json::Value>,
    reasoning_streamed: bool,
    opened: std::collections::BTreeSet<u64>,
    done: std::collections::BTreeMap<u64, serde_json::Value>,
    scrubber: Option<UpstreamErrorScrubber>,
}

#[derive(Default)]
pub(super) struct BridgeCapture(Mutex<Source>);

impl BridgeCapture {
    /// Full terminal output owns private continuation state. Stream policy must
    /// not rewrite actionable text/calls and then silently restore their original
    /// values from that terminal. Refuse such a mismatch before tool admission.
    pub(super) fn complete_observed(
        &self,
        executor: &HttpExecutor,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
        folded: GenerateResult,
    ) -> Result<GenerateResult> {
        fn visible(result: &GenerateResult, reasoning_streamed: bool) -> Vec<serde_json::Value> {
            result
                .content
                .iter()
                .filter_map(|part| match part {
                    Content::Text { text, .. } if !text.is_empty() => {
                        Some(serde_json::json!({"text":text}))
                    }
                    Content::Reasoning { text, .. } if reasoning_streamed => {
                        Some(serde_json::json!({"reasoning":text}))
                    }
                    Content::ToolCall {
                        id,
                        name,
                        arguments,
                        provider_executed: false,
                        ..
                    } => Some(serde_json::json!({"id":id,"name":name,"arguments":arguments})),
                    _ => None,
                })
                .collect()
        }
        let reasoning_streamed = self.source().reasoning_streamed;
        let observed = visible(&folded, reasoning_streamed);
        let complete = self.complete(executor, target, prompt, ctx, folded)?;
        if observed != visible(&complete, reasoning_streamed) {
            return Err(BitrouterError::UpstreamInvalidResponse {
                message: "managed stream and complete terminal output differ".into(),
                usage: complete.usage.clone().map(Box::new),
            });
        }
        Ok(complete)
    }

    fn source(&self) -> std::sync::MutexGuard<'_, Source> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn authenticated(
        &self,
        request: &reqwest::Request,
        authority: Option<ContinuationAuthority>,
        prompt: &Prompt,
    ) -> Result<()> {
        let wire: Option<serde_json::Value> = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .and_then(|bytes| serde_json::from_slice(bytes).ok());
        let effort = serde_json::to_value(prompt.params.reasoning_effort)
            .map_err(|_| ContinuationFailure::BindingChanged.error())?;
        let mut source = self.source();
        source.authority = authority;
        source.storage_allowed = wire
            .as_ref()
            .is_some_and(|body| body.get("store") != Some(&serde_json::Value::Bool(false)));
        source.effort_bound = wire.as_ref().is_some_and(|body| {
            body.get("reasoning")
                .and_then(|reasoning| reasoning.get("effort"))
                .unwrap_or(&serde_json::Value::Null)
                == &effort
        });
        Ok(())
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
                                usage: bitrouter_ai::protocol::OutboundAdapter::parse_response(
                                    &bitrouter_ai::protocol::responses::ResponsesAdapter,
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
        prompt: &Prompt,
        ctx: &PipelineContext,
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
        let replayable = bitrouter_ai::protocol::responses::output_replayable(&terminal);
        let stored =
            source.storage_allowed && terminal.get("store") == Some(&serde_json::Value::Bool(true));
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
            if ctx
                .extension::<super::super::native_context::NativePrivateContextRuntime>()
                .is_some()
            {
                super::super::native_continuation::mark_required_state(&mut folded.content);
            }
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
        if let Some(runtime) =
            ctx.extension::<super::super::native_context::NativePrivateContextRuntime>()
        {
            if !replayable {
                super::super::native_continuation::mark_required_state(&mut folded.content);
            }
            runtime.succeeded(prompt, target, source.authority.take(), &folded);
            runtime.stored_response(stored, replayable, source.effort_bound);
        }
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
        error_scrubber.capture_effective_target_key(target);
        error_scrubber.redactor.capture_prompt_continuity(prompt);
        let url = transport.endpoint_url(&target.model_target(), true);
        let trace_headers = ctx.take_outbound_trace_headers();

        let (client, _) = self.client_for(
            target,
            ctx.extension::<super::super::native::NativeManagedRequest>()
                .is_some(),
        );

        let mut attempted_auth_refresh = false;
        let response = loop {
            let mut body =
                self.render_execution_request(adapter.as_ref(), target, prompt, ctx, true)?;
            let managed_expected = self.managed_expected_body(&body, target, ctx)?;
            if self.auth_appliers.lookup(&target.provider_name).is_some() {
                native_work::observe(
                    ctx,
                    NativeProviderWorkKind::AuthenticationPreparation,
                    self.shape_request_body(&mut body, target),
                    |_| None,
                )
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
                managed_expected: managed_expected.as_ref(),
                target,
                transport,
                ctx,
                trace_headers: trace_headers.as_ref(),
            };

            let applied = self
                .build_authenticated_request(&request_input)
                .await
                .map_err(|error| error_scrubber.scrub_error(error))?;
            let (request, authority) = applied.into_parts();
            if let Some(capture) = &capture {
                capture.authenticated(&request, authority, prompt)?;
            }
            error_scrubber.capture_request_credentials(&request, target);
            let rejected_authorization = request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .cloned();
            let response = native_work::observe(
                ctx,
                NativeProviderWorkKind::HttpDispatch,
                async {
                    record_native_dispatch(prompt, target, ctx)?;
                    client
                        .send(request, &CancellationToken::new())
                        .await
                        .map_err(|error| error_scrubber.scrub_error(error.into()))
                },
                |response| Some(response.status().as_u16()),
            )
            .await?;

            let status = response.status();
            let retry_after =
                parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER));
            if status.is_success() {
                if let Some(capture) = &capture {
                    capture.source().scrubber = Some(error_scrubber.clone());
                }
                break response;
            }
            let text = response_body::read(response, ctx)
                .await
                .map_err(|error| error_scrubber.scrub_error(error))?;
            if status == reqwest::StatusCode::UNAUTHORIZED
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
        response_body::check_length(&response, limit)?;
        let byte_stream = response_body::bounded(response.bytes_stream(), limit);

        let stream = async_stream::stream! {
            use eventsource_stream::Eventsource;
            let mut terminal = false;
            let events = byte_stream.eventsource();
            futures::pin_mut!(events);
            while let Some(event) = events.next().await {
                match event {
                    Ok(ev) => {
                        let sse = SseEvent {
                            event: if ev.event.is_empty() { None } else { Some(ev.event) },
                            data: ev.data,
                        };
                        match decoder.decode(&sse) {
                            Ok(parts) => {
                                if let Some(capture) = &capture
                                    && let Err(error) = capture.terminal(&sse) {
                                    yield Err(error_scrubber.scrub_error(error));
                                    return;
                                }
                                for p in parts {
                                    terminal |= p.is_terminal();
                                    yield Ok(p);
                                }
                            }
                            Err(e) => {
                                let mut error = classify_stream_decoder_error(e);
                                // Match direct AI calls: retain independently valid usage
                                // even when the rest of a Chat event cannot be decoded.
                                if protocol == ApiProtocol::ChatCompletions
                                    && let Ok(chunk) = serde_json::from_str::<serde_json::Value>(&sse.data)
                                    && let Some(reported) = chunk.get("usage").and_then(bitrouter_ai::protocol::chat_completions::parse_usage)
                                {
                                    yield Ok(StreamPart::Usage { usage: reported.clone() });
                                    if let BitrouterError::UpstreamInvalidResponse { usage, .. } = &mut error { *usage = Some(Box::new(reported)); }
                                }
                                yield Err(error_scrubber.scrub_error(error));
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
                        yield Err(error_scrubber.scrub_error(error));
                        return;
                    }
                }
            }
            match decoder.finish() {
                Ok(parts) => {
                    for p in parts {
                        terminal |= p.is_terminal();
                        yield Ok(p);
                    }
                }
                Err(e) => {
                    yield Err(error_scrubber.scrub_error(classify_stream_decoder_error(e)));
                    return;
                }
            }
            if !terminal {
                yield Err(BitrouterError::UpstreamInvalidResponse { message: "upstream stream ended without a model terminal part".into(), usage: None });
            }
        };

        Ok(Box::pin(bitrouter_ai::providers::google_chat::bind_stream(
            stream,
            target.model_target(),
        )))
    }
}

#[cfg(test)]
mod lite_tests {
    use super::*;
    use bitrouter_ai::protocol::OutboundAdapter;
    use bitrouter_ai::protocol::responses::ResponsesAdapter;
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
