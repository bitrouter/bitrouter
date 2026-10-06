//! Complete Responses output and actual request provenance for the native SSE bridge.
//! <https://developers.openai.com/api/docs/guides/streaming-responses>

use super::*;

#[derive(Default)]
struct Source {
    authority: Option<ContinuationAuthority>,
    storage_allowed: bool,
    effort_bound: bool,
    terminal: Option<serde_json::Value>,
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
        let replayable = super::super::protocol::responses::output_replayable(&terminal);
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
            .await?;
        }
        let continuation_substitution = apply_provider_continuation(&mut body, target, ctx)?;
        let mut error_scrubber = UpstreamErrorScrubber::new(continuation_substitution);
        error_scrubber.capture_effective_target_key(target);
        let url = transport.endpoint_url(target, true);
        let trace_headers = ctx.take_outbound_trace_headers();

        let (client, timeouts) = self.client_for(
            target,
            ctx.extension::<super::super::native::NativeManagedRequest>()
                .is_some(),
        );
        let request_input = RequestBuildInput {
            client: &client,
            timeouts: &timeouts,
            url: &url,
            body: &body,
            managed_expected: managed_expected.as_ref(),
            target,
            transport,
            ctx,
            trace_headers: trace_headers.as_ref(),
        };
        let mut attempted_auth_refresh = false;
        let response = loop {
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
                    client.execute(request).await.map_err(|error| {
                        let error = if error.is_timeout() {
                            BitrouterError::UpstreamTimeout
                        } else {
                            BitrouterError::Upstream {
                                status: 502,
                                message: format!(
                                    "stream request to {} failed: {error}",
                                    target.provider_name
                                ),
                            }
                        };
                        error_scrubber.scrub_error(error)
                    })
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
        let limit = response_body::limit(ctx);
        response_body::check_length(&response, limit)?;
        let byte_stream = response_body::bounded(response.bytes_stream(), limit);

        let stream = async_stream::stream! {
            use eventsource_stream::Eventsource;
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
                                    yield Ok(p);
                                }
                            }
                            Err(e) => {
                                yield Err(error_scrubber.scrub_error(
                                    classify_stream_decoder_error(e)
                                ));
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
                        yield Ok(p);
                    }
                }
                Err(e) => yield Err(error_scrubber.scrub_error(
                    classify_stream_decoder_error(e)
                )),
            }
        };

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod lite_tests {
    use super::*;
    use crate::language_model::protocol::OutboundAdapter;
    use crate::language_model::protocol::responses::ResponsesAdapter;

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
