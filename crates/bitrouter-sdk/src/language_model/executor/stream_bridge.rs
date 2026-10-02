//! Complete Responses output and actual request provenance for the native SSE bridge.
//! https://developers.openai.com/api/docs/guides/streaming-responses

use super::*;

#[derive(Default)]
struct Source {
    authority: Option<ContinuationAuthority>,
    storage_allowed: bool,
    effort_bound: bool,
    terminal: Option<serde_json::Value>,
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

    fn terminal(&self, event: &SseEvent) {
        let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&event.data) else {
            return;
        };
        let event_type = json
            .get("type")
            .and_then(serde_json::Value::as_str)
            .or(event.event.as_deref());
        if matches!(
            event_type,
            Some("response.completed" | "response.incomplete")
        ) {
            self.source().terminal = json.get_mut("response").map(serde_json::Value::take);
        }
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
            let text = response.text().await.map_err(|error| {
                error_scrubber.scrub_error(upstream_body_error(
                    "reading upstream stream error body",
                    error,
                ))
            })?;
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
        let byte_stream = response.bytes_stream();

        let stream = async_stream::stream! {
            use eventsource_stream::Eventsource;
            let mut events = byte_stream.eventsource();
            while let Some(event) = events.next().await {
                match event {
                    Ok(ev) => {
                        let sse = SseEvent {
                            event: if ev.event.is_empty() { None } else { Some(ev.event) },
                            data: ev.data,
                        };
                        match decoder.decode(&sse) {
                            Ok(parts) => {
                                if let Some(capture) = &capture {
                                    capture.terminal(&sse);
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
                        let is_timeout = matches!(
                            &e,
                            eventsource_stream::EventStreamError::Transport(re) if re.is_timeout()
                        );
                        yield Err(error_scrubber.scrub_error(
                            stream_transport_error(is_timeout, &e)
                        ));
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
