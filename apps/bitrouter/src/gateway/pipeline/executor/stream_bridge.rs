//! Complete Responses output and actual request provenance for the native SSE bridge.
//! <https://developers.openai.com/api/docs/guides/streaming-responses>

use super::*;
use bitrouter_ai::types::Content;

#[derive(Default)]
struct Source {
    tenant_binding: Option<super::tenancy::BoundAuthentication>,
    tenant_attempt: Option<bitrouter_ai::tenancy::identity::AttemptId>,
    canonical_prompt: Option<[u8; 32]>,
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

fn tenant_native_responses(target: &RoutingTarget, ctx: &PipelineContext) -> bool {
    target.api_protocol == ApiProtocol::Responses
        && super::tenancy::has_execution(ctx)
        && ctx
            .extension::<super::super::native::NativeManagedRequest>()
            .is_some()
}

fn stream_binding_error() -> BitrouterError {
    super::super::native_continuation::continuation_error(ContinuationFailure::BindingChanged)
}

/// The bridge may force SSE without changing the admitted canonical request.
/// Exhaustive fields prevent a future Prompt field from silently escaping this
/// check. Borrowed comparison adds no prompt/history allocation.
fn verify_native_stream_prompt(transport: &Prompt, canonical: &Prompt) -> Result<()> {
    let Prompt {
        model,
        system,
        system_provider_metadata,
        messages,
        tools,
        params,
        response_format,
        tool_choice,
        stream,
    } = transport;
    let Prompt {
        model: expected_model,
        system: expected_system,
        system_provider_metadata: expected_metadata,
        messages: expected_messages,
        tools: expected_tools,
        params: expected_params,
        response_format: expected_format,
        tool_choice: expected_choice,
        stream: expected_stream,
    } = canonical;
    if (
        model,
        system,
        system_provider_metadata,
        messages,
        tools,
        params,
        response_format,
        tool_choice,
    ) != (
        expected_model,
        expected_system,
        expected_metadata,
        expected_messages,
        expected_tools,
        expected_params,
        expected_format,
        expected_choice,
    ) || (*expected_stream && !*stream)
    {
        return Err(stream_binding_error());
    }
    Ok(())
}

fn verify_native_stream_request(
    transport: &Prompt,
    canonical: &Prompt,
    wire: Option<&serde_json::Value>,
) -> Result<()> {
    verify_native_stream_prompt(transport, canonical)?;
    // Authenticated request bytes retain stream=true; the source proof does not
    // substitute the canonical flag for the actual dispatch/send gate digest.
    if wire.and_then(|body| body.get("stream")) != Some(&serde_json::Value::Bool(true)) {
        return Err(stream_binding_error());
    }
    Ok(())
}

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
        target: &RoutingTarget,
        ctx: &PipelineContext,
    ) -> Result<()> {
        let wire: Option<serde_json::Value> = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .and_then(|bytes| serde_json::from_slice(bytes).ok());
        let canonical_prompt = if tenant_native_responses(target, ctx) {
            let canonical = ctx.generation_prompt().ok_or_else(stream_binding_error)?;
            // The canonical prompt may be nonstreaming while its verified
            // executor transport uses SSE. Prove that actual request separately.
            verify_native_stream_request(prompt, canonical, wire.as_ref())?;
            Some(
                super::tenancy::native::prompt_digest(canonical)
                    .ok_or_else(stream_binding_error)?,
            )
        } else {
            None
        };
        let effort = serde_json::to_value(prompt.params.reasoning_effort).map_err(|_| {
            crate::gateway::pipeline::native_continuation::continuation_error(
                ContinuationFailure::BindingChanged,
            )
        })?;
        let mut source = self.source();
        source.canonical_prompt = canonical_prompt;
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
        let replayable = crate::gateway::model::protocol::responses::output_replayable(&terminal);
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
            let prompt = if tenant_native_responses(target, ctx) {
                let Some(canonical) = ctx.generation_prompt() else {
                    return Ok(folded);
                };
                if verify_native_stream_prompt(prompt, canonical).is_err()
                    || source.canonical_prompt.is_none()
                    || super::tenancy::native::prompt_digest(canonical) != source.canonical_prompt
                {
                    // Keep already observed output/usage, but issue no proof
                    // from a canonical view changed since request validation.
                    return Ok(folded);
                }
                canonical
            } else {
                prompt
            };
            if let (Some(binding), Some(attempt), Some(authority)) = (
                source.tenant_binding.as_ref(),
                source.tenant_attempt.as_ref(),
                source.authority.as_ref(),
            ) {
                binding.record_native_source(
                    attempt.clone(),
                    authority,
                    prompt,
                    &folded,
                    super::tenancy::native::Storage {
                        stored,
                        replayable,
                        effort_bound: source.effort_bound,
                    },
                );
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
        error_scrubber.capture_target(target, ctx);
        error_scrubber.redactor.capture_prompt_continuity(prompt);
        let url = transport.endpoint_url(&target.model_target(), true);
        let trace_headers = ctx.take_outbound_trace_headers();

        let (client, _) = self.client_for(
            target,
            ctx.extension::<super::super::native::NativeManagedRequest>()
                .is_some()
                || super::shared::has_admission(ctx)
                || super::tenancy::has_execution(ctx),
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
            let managed_expected = self.managed_expected_body(&body, target, ctx)?;
            if self.needs_body_shaping(target, ctx)? {
                native_work::observe(
                    ctx,
                    NativeProviderWorkKind::AuthenticationPreparation,
                    self.shape_request_body(&mut body, target, ctx),
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
            let mut shared_attempt =
                super::shared::Attempt::begin(self.shared_authority, &applied, target, ctx).await?;
            let tenant_binding = applied.tenant_binding();
            let (mut request, authority, mut tenant_attempt) = applied.into_parts();
            if let Some(capture) = &capture {
                capture.authenticated(&request, authority.clone(), prompt, target, ctx)?;
            }
            if let Some(source) = &mut http_source {
                source.authenticated(&request, authority, prompt);
            }

            error_scrubber.capture_request_credentials(&request, target);
            let rejected_authorization = request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .cloned();
            let mut native_attempt = None;
            let response = native_work::observe(
                ctx,
                NativeProviderWorkKind::HttpDispatch,
                async {
                    let routing_guard = if super::tenancy::has_execution(ctx) {
                        Some(crate::tenancy::routing::admit(ctx, target)?)
                    } else {
                        None
                    };
                    record_native_dispatch(prompt, target, ctx)?;
                    native_attempt = super::tenancy::admit_send(
                        tenant_binding.as_ref(),
                        &mut tenant_attempt,
                        &mut request,
                        target,
                        ctx,
                    )?;
                    if let Some(guard) = routing_guard {
                        guard.sent()?;
                    }
                    client
                        .send(request, &super::tenancy::cancellation(ctx)?)
                        .await
                        .map_err(|error| error_scrubber.scrub_error(error.into()))
                },
                |response| Some(response.status().as_u16()),
            )
            .await?;

            let status = response.status();
            receipt::status(&mut tenant_attempt, status.as_u16())?;
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
                    source.tenant_binding = tenant_binding;
                    source.tenant_attempt = native_attempt;
                }
                break (response, shared_attempt, tenant_attempt);
            }
            shared_attempt.rejected(status.as_u16()).await?;
            let text = response_body::read(response, ctx)
                .await
                .map_err(|error| error_scrubber.scrub_error(error))?;
            receipt::error_json_text(&mut tenant_attempt, &target.api_protocol, &text)?;
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
        let cancellation = super::tenancy::cancellation(ctx)?;
        response_body::check_length(&response, limit)?;
        let byte_stream = response_body::bounded(response.bytes_stream(), limit);

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
                                    if let Some(source) = &mut http_source { source.delivered.observe(&p); }
                                    terminal |= p.is_terminal();
                                    if (shared_attempt.is_shared() || tenant_attempt.is_some() || http_source.is_some()) && p.is_terminal() { held_terminal = Some(p); continue; }
                                    yield Ok(p);
                                }
                            }
                            Err(e) => {
                                // Retain valid counts from this same event before
                                // persisting its failure or yielding to a caller.
                                let reported = error_event_usage(&protocol, &sse, &e);
                                if let Some(usage) = &reported {
                                    shared_attempt.observe(&StreamPart::Usage { usage: usage.clone() });
                                }
                                if matches!(&e, crate::gateway::model::error::ModelError::Provider { .. } | crate::gateway::model::error::ModelError::PolicyViolation { .. })
                                    && let Err(error) = shared_attempt.failed_stream().await
                                {
                                    drop(tenant_attempt.take()); yield Err(error);
                                    return;
                                }
                                if matches!(&e, crate::gateway::model::error::ModelError::Provider { .. } | crate::gateway::model::error::ModelError::PolicyViolation { .. })
                                    && let Err(error) = receipt::event(&mut tenant_attempt, &protocol, &sse).and_then(|()| receipt::complete(&mut tenant_attempt, false))
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
                        if let Some(source) = &mut http_source { source.delivered.observe(&p); }
                        terminal |= p.is_terminal();
                        if (shared_attempt.is_shared() || tenant_attempt.is_some() || http_source.is_some()) && p.is_terminal() { held_terminal = Some(p); continue; }
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

    #[test]
    fn native_sse_binding_allows_only_the_transport_flag_and_requires_actual_sse()
    -> anyhow::Result<()> {
        use bitrouter_ai::types::{Message, Role};
        let canonical = Prompt {
            model: "literal-route".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "original")],
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let wire = serde_json::json!({"stream":true});
        let mut transport = canonical.clone();
        transport.stream = true;
        verify_native_stream_request(&transport, &canonical, Some(&wire))?;
        // Forced-SSE providers can render stream=true from a canonical prompt
        // without an intermediate stream=true Prompt clone.
        verify_native_stream_request(&canonical, &canonical, Some(&wire))?;
        for invalid in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({"stream":false})),
        ] {
            assert!(
                verify_native_stream_request(&transport, &canonical, invalid.as_ref()).is_err()
            );
        }
        for (field, changed) in [
            ("model", serde_json::json!("other")),
            ("system", serde_json::json!("changed instruction")),
            (
                "system_provider_metadata",
                serde_json::json!({"provider":{"changed":true}}),
            ),
            (
                "messages",
                serde_json::to_value(vec![Message::text(Role::User, "changed")])?,
            ),
            (
                "tools",
                serde_json::json!([{"type":"function","name":"changed","parameters":{"type":"object"}}]),
            ),
            ("params", serde_json::json!({"max_tokens":7})),
            (
                "response_format",
                serde_json::json!({"type":"json_schema","schema":{"type":"object"}}),
            ),
            ("tool_choice", serde_json::json!("required")),
        ] {
            let mut value = serde_json::to_value(&transport)?;
            value[field] = changed;
            let changed: Prompt = serde_json::from_value(value)?;
            assert!(
                verify_native_stream_request(&changed, &canonical, Some(&wire)).is_err(),
                "{field}"
            );
        }
        // The actual prompt proof stays exact; neither the hash function nor
        // NativePrivateContextRuntime's final canonical equality is weakened.
        assert_ne!(
            super::super::tenancy::native::prompt_digest(&transport),
            super::super::tenancy::native::prompt_digest(&canonical)
        );
        Ok(())
    }

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
