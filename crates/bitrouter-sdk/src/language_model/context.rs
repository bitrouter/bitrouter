//! Pipeline contexts for the `language_model` protocol: `PipelineContext` (the
//! whole-request water-flow context) and `StreamContext` (the StreamHook-stage
//! view borrowed from it). `SettlementContext` lives in
//! [`crate::language_model::settlement`].

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::caller::CallerContext;
use crate::event::{EventBus, PipelineEvent};
use crate::language_model::settlement::SettlementContext;
use crate::language_model::stream::UsageAccumulator;
use crate::language_model::timing::{
    FirstTokenKind, FirstTokenTiming, duration_millis, elapsed_millis,
};
use crate::language_model::types::{
    ApiProtocol, ChatStreamOptions, Content, ExecutionResult, FinishReason, PipelineRequest,
    PipelineResponse, Prompt, RoutingTarget, StreamPart, Usage,
};
use crate::plugin::PluginId;

/// A type-keyed map of request-scoped extension values. Each entry is an
/// `Arc<T>` keyed by `T`'s `TypeId`, so cloning the map is a handful of
/// refcount bumps — cheap enough to copy from the [`PipelineContext`] into the
/// per-stream [`StreamContext`]. This is the typed counterpart to the JSON
/// `metadata` channel: it carries an *already-built* per-request value (e.g. a
/// compiled guardrail rule set) that a Stage-1 hook resolves and a later
/// [`StreamHook`](crate::language_model::StreamHook) reads on the hot path
/// without re-parsing.
#[derive(Clone, Default)]
struct Extensions {
    map: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl Extensions {
    fn insert<T: Send + Sync + 'static>(&mut self, value: Arc<T>) {
        self.map.insert(TypeId::of::<T>(), value);
    }

    fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.map
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|v| v.downcast::<T>().ok())
    }
}

/// Count media content blocks in a content stream (observability only).
///
/// Only `Content::File` is treated as media — it covers all non-text IANA
/// media types (images, audio, video, documents). `Content::Text`,
/// `Content::Reasoning`, and `Content::ToolCall`/`Content::ToolResult` are
/// not media.
fn count_media<'a>(blocks: impl Iterator<Item = &'a Content>) -> u64 {
    blocks.filter(|c| matches!(c, Content::File { .. })).count() as u64
}

/// The whole-request pipeline context. Follows a water-flow model: data flows
/// downstream, each stage appends, downstream may read everything upstream
/// wrote but never mutate it.
pub struct PipelineContext {
    // ===== original request (Stage 0, read-only) =====
    request_id: String,
    model: String,
    caller: CallerContext,
    headers: http::HeaderMap,
    prompt: Prompt,
    /// The inbound wire protocol (Stage 0). Route resolution uses it to prefer
    /// a native, same-protocol upstream. `None` when the request was built
    /// without a known inbound protocol.
    inbound_protocol: Option<ApiProtocol>,
    request_started_at: Instant,

    // ===== accumulated: written per stage, readable downstream =====
    /// The resolved fallback chain (Stage 2).
    pub route_chain: Option<Vec<RoutingTarget>>,
    /// Most recent upstream target whose execution was started. A failed
    /// request has no [`ExecutionResult`], but settlement still needs the
    /// attempted provider/model identity for reliability and authoritative
    /// receipt reconciliation.
    last_attempted_target: Mutex<Option<RoutingTarget>>,
    /// The execution result (Stage 3). Stored here rather than moved out so
    /// Settlement can borrow it without an ownership fight.
    pub execution_result: Option<ExecutionResult>,
    stream_provider_started_at: Option<Instant>,
    first_token_timing: Option<FirstTokenTiming>,
    generation_duration_ms: Option<u64>,
    finalized_request_duration_ms: Option<u64>,

    // ===== plugin extension data =====
    metadata: HashMap<PluginId, serde_json::Value>,
    /// Typed, request-scoped extensions. Unlike `metadata` (JSON, per-stage),
    /// these carry an already-built `Arc<T>` and are propagated into the
    /// per-stream [`StreamContext`] so a value resolved in a pre-request hook
    /// is readable from the stream stage.
    extensions: Extensions,

    // ===== typed event bus =====
    events: EventBus,

    /// Outbound HTTP headers stashed by an `ObserveHook::on_hop_start` for
    /// the executor to merge into the next upstream request. The slot is
    /// cleared on `take_outbound_trace_headers`, so each hop starts clean.
    ///
    /// Carrier for W3C trace-context propagation
    /// (`traceparent` / `tracestate`) without coupling the SDK to
    /// OpenTelemetry types.
    ///
    /// Spec: <https://www.w3.org/TR/trace-context/>
    outbound_trace_headers: Mutex<Option<http::HeaderMap>>,
}

impl PipelineContext {
    /// Build a fresh context from an inbound request.
    pub fn new(req: PipelineRequest) -> Self {
        Self {
            request_id: req.request_id,
            model: req.model,
            caller: req.caller,
            headers: req.headers,
            prompt: req.prompt,
            inbound_protocol: req.inbound_protocol,
            request_started_at: Instant::now(),
            route_chain: None,
            last_attempted_target: Mutex::new(None),
            execution_result: None,
            stream_provider_started_at: None,
            first_token_timing: None,
            generation_duration_ms: None,
            finalized_request_duration_ms: None,
            metadata: HashMap::new(),
            extensions: Extensions::default(),
            events: EventBus::new(),
            outbound_trace_headers: Mutex::new(None),
        }
    }

    /// Fork the immutable request and accumulated hook state for a later
    /// server-tool model turn. The fork keeps the original request identity,
    /// headers, protocol, metadata, extensions, events, and resolved route;
    /// only the evolving prompt and per-turn execution/trace state are fresh.
    pub(crate) fn fork_for_prompt(&self, prompt: Prompt) -> Self {
        Self {
            request_id: self.request_id.clone(),
            model: self.model.clone(),
            caller: self.caller.clone(),
            headers: self.headers.clone(),
            prompt,
            inbound_protocol: self.inbound_protocol.clone(),
            request_started_at: self.request_started_at,
            route_chain: self.route_chain.clone(),
            last_attempted_target: Mutex::new(None),
            execution_result: None,
            stream_provider_started_at: None,
            first_token_timing: None,
            generation_duration_ms: None,
            finalized_request_duration_ms: None,
            metadata: self.metadata.clone(),
            extensions: self.extensions.clone(),
            events: self.events.clone(),
            outbound_trace_headers: Mutex::new(None),
        }
    }

    fn serving_target(&self) -> Option<RoutingTarget> {
        let chain = self.route_chain.as_ref()?;
        if let Some(target) = self.execution_result.as_ref().and_then(|execution| {
            chain.iter().find(|target| {
                target.provider_name == execution.provider_id
                    && target.service_id == execution.model_id
                    && target.account_label == execution.account_label
            })
        }) {
            return Some(target.clone());
        }
        self.last_attempted_target()
            .or_else(|| chain.first().cloned())
    }

    pub(crate) fn set_last_attempted_target(&self, target: RoutingTarget) {
        match self.last_attempted_target.lock() {
            Ok(mut current) => *current = Some(target),
            Err(poisoned) => *poisoned.into_inner() = Some(target),
        }
    }

    fn last_attempted_target(&self) -> Option<RoutingTarget> {
        match self.last_attempted_target.lock() {
            Ok(current) => current.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub(crate) fn set_stream_provider_started_at(&mut self, started_at: Instant) {
        self.stream_provider_started_at = Some(started_at);
    }

    pub(crate) fn finalize_request_duration(&mut self) {
        let request_duration_ms = elapsed_millis(self.request_started_at);
        self.finalized_request_duration_ms = Some(request_duration_ms);
        if let Some(result) = self.execution_result.as_mut() {
            result.request_duration_ms = request_duration_ms;
        }
    }

    /// End-to-end request duration in milliseconds. Once settlement begins this is
    /// the finalized value; before then it is the current elapsed duration.
    pub fn request_duration_ms(&self) -> u64 {
        self.finalized_request_duration_ms
            .or_else(|| {
                self.execution_result
                    .as_ref()
                    .map(|result| result.request_duration_ms)
                    .filter(|duration| *duration > 0)
            })
            .unwrap_or_else(|| elapsed_millis(self.request_started_at))
    }

    pub(crate) fn finalize_stream_upstream_duration(&mut self) {
        let Some(provider_started_at) = self.stream_provider_started_at else {
            return;
        };
        if let Some(result) = self.execution_result.as_mut() {
            result.upstream_duration_ms = Some(elapsed_millis(provider_started_at));
        }
    }

    /// The first semantic output timing captured for a streamed request.
    pub fn first_token_timing(&self) -> Option<FirstTokenTiming> {
        self.first_token_timing
    }

    /// Time from the first to the last semantic stream delta.
    pub fn generation_duration_ms(&self) -> Option<u64> {
        self.generation_duration_ms
    }

    /// Store outbound HTTP headers for the next upstream request. The
    /// executor merges them into the request just before issuing it.
    /// Typically called by an `ObserveHook` from `on_hop_start` to inject
    /// W3C trace context. Replaces any previously-set headers (whole-map
    /// overwrite, not merge).
    pub fn set_outbound_trace_headers(&self, headers: http::HeaderMap) {
        let mut slot = match self.outbound_trace_headers.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = Some(headers);
    }

    /// Take any pending outbound trace headers. Called by the executor
    /// right before issuing the upstream request — clears the slot so a
    /// subsequent hop in the same request starts with no leftover headers.
    pub fn take_outbound_trace_headers(&self) -> Option<http::HeaderMap> {
        let mut slot = match self.outbound_trace_headers.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        slot.take()
    }

    /// The request id.
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// The requested model string (may still carry `@preset` / `:variant`).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The authenticated / synthesised caller.
    pub fn caller(&self) -> &CallerContext {
        &self.caller
    }

    /// Replace the caller. The one Stage-1 exception to the water-flow model:
    /// an `AuthHook` validates credentials and upgrades the pre-auth anonymous
    /// placeholder to the real authenticated identity. No later stage may call
    /// this — by Stage 2 the caller is established and read-only.
    pub fn set_caller(&mut self, caller: CallerContext) {
        self.caller = caller;
    }

    /// Inbound HTTP headers.
    pub fn headers(&self) -> &http::HeaderMap {
        &self.headers
    }

    /// The canonical request body.
    pub fn prompt(&self) -> &Prompt {
        &self.prompt
    }

    /// The inbound wire protocol the request arrived on, if known. Route
    /// resolution uses it to prefer a native (same-protocol) upstream.
    pub fn inbound_protocol(&self) -> Option<ApiProtocol> {
        self.inbound_protocol.clone()
    }

    /// Replace the canonical model name (used after preset/variant stripping).
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
    }

    /// Apply preset prompt-body overrides. `system_prompt`, when
    /// present, replaces the prompt's system; `params` is shallow-merged into
    /// the prompt's supplemental params so provider-specific knobs survive
    /// without inheriting the inbound wire's ownership. Already-set request
    /// fields take precedence — a preset is a *default*, not an override of an
    /// explicit request value (except for `system` which has no merging
    /// surface).
    pub fn apply_preset_overrides(
        &mut self,
        overrides: &crate::language_model::routing::PromptOverrides,
    ) {
        if overrides.is_empty() {
            return;
        }
        if let Some(system) = &overrides.system_prompt {
            // Only fill `system` if the request did not already set one.
            if self.prompt.system.is_none() {
                self.prompt.system = Some(system.clone());
            }
        }
        for (k, v) in &overrides.params {
            // Don't clobber an explicit request value; presets are defaults.
            if self.prompt.params.extra.contains_key(k) {
                continue;
            }

            // Promote the protocol fields normalized by inbound adapters into
            // the same typed slots when they come from a preset. Otherwise a
            // preset `store: true`, for example, could bypass the cross-wire
            // safety checks by remaining an untyped supplemental field.
            match k.as_str() {
                "store" => {
                    if self.prompt.params.store.is_none() {
                        self.prompt.params.store = v.as_bool();
                    }
                    if self.prompt.params.store.is_some() {
                        continue;
                    }
                }
                "parallel_tool_calls" => {
                    if self.prompt.params.parallel_tool_calls.is_none() {
                        self.prompt.params.parallel_tool_calls = v.as_bool();
                    }
                    if self.prompt.params.parallel_tool_calls.is_some() {
                        continue;
                    }
                }
                "stream_options" => {
                    if self.prompt.params.chat_stream_options.is_none()
                        && let Ok(options) = serde_json::from_value::<ChatStreamOptions>(v.clone())
                    {
                        self.prompt.params.chat_stream_options = Some(options);
                    }
                    if self.prompt.params.chat_stream_options.is_some() {
                        continue;
                    }
                }
                _ => {}
            }

            self.prompt
                .params
                .supplemental_extra
                .entry(k.clone())
                .or_insert_with(|| v.clone());
        }
    }

    /// Write this plugin's metadata blob.
    pub fn set_metadata(&mut self, plugin_id: &PluginId, value: serde_json::Value) {
        self.metadata.insert(plugin_id.clone(), value);
    }

    /// Read another plugin's metadata blob.
    pub fn get_metadata(&self, plugin_id: &PluginId) -> Option<&serde_json::Value> {
        self.metadata.get(plugin_id)
    }

    /// The full per-request metadata map. Used to snapshot a request's
    /// plugin-scoped state into an owned
    /// [`ToolContext`](crate::language_model::server_tools::toolset::ToolContext)
    /// for a server-tool execution, which may outlive this borrow (e.g. on the
    /// streaming path).
    pub fn metadata(&self) -> &HashMap<PluginId, serde_json::Value> {
        &self.metadata
    }

    /// Insert a typed, request-scoped extension, replacing any existing value
    /// of the same type. The value is shared (`Arc`) and copied into the
    /// per-stream [`StreamContext`], so a Stage-1 hook can deposit a value that
    /// the downstream [`StreamHook`](crate::language_model::StreamHook) reads —
    /// the channel JSON `metadata` can't provide because it neither survives
    /// into the stream stage nor holds non-serialisable values.
    pub fn insert_extension<T: Send + Sync + 'static>(&mut self, value: Arc<T>) {
        self.extensions.insert(value);
    }

    /// Read a typed, request-scoped extension previously inserted with
    /// [`insert_extension`](Self::insert_extension).
    pub fn extension<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.extensions.get()
    }

    /// Emit a typed pipeline event.
    pub fn emit<E: PipelineEvent>(&mut self, event: E) {
        self.events.emit(event);
    }

    /// Whether an event of type `E` was emitted.
    pub fn has_event<E: PipelineEvent>(&self) -> bool {
        self.events.has::<E>()
    }

    /// The first emitted event of type `E`.
    pub fn get_event<E: PipelineEvent>(&self) -> Option<&E> {
        self.events.get::<E>()
    }

    /// All emitted events of type `E`.
    pub fn get_events<E: PipelineEvent>(&self) -> Vec<&E> {
        self.events.get_all::<E>()
    }

    /// Shared access to the event bus (e.g. for `dump_json` into a receipt).
    pub fn events(&self) -> &EventBus {
        &self.events
    }

    /// Rough `char` count of model-visible prompt payloads — system instruction
    /// plus message text, reasoning, tool call args, and tool results. Used to
    /// seed the [`UsageAccumulator`] so a stream without an upstream usage frame
    /// can still estimate prompt tokens. The estimate is deliberately rough but
    /// should not collapse tool-heavy agent follow-up turns to `0`.
    fn prompt_text_chars(&self) -> u64 {
        let prompt = self.prompt();
        let mut chars = prompt
            .system
            .as_ref()
            .map_or(0u64, |s| s.chars().count() as u64);
        for message in &prompt.messages {
            for content in &message.content {
                chars = chars.saturating_add(prompt_content_chars(content));
            }
        }
        chars
    }

    /// Borrow a `StreamContext` for the StreamHook stage.
    pub fn stream_context(&self) -> StreamContext {
        let target = self.serving_target();
        StreamContext {
            request_id: self.request_id.clone(),
            caller: self.caller.clone(),
            target,
            accumulated_usage: UsageAccumulator::with_prompt_chars(self.prompt_text_chars()),
            parts_emitted: 0,
            final_usage: None,
            provider_started_at: self.stream_provider_started_at,
            first_token_timing: self.first_token_timing,
            first_semantic_at: None,
            generation_duration_ms: None,
            finish_reason: None,
            events: EventBus::new(),
            metadata: HashMap::new(),
            // Refcount-bump copy: a value a pre-request hook deposited (e.g. the
            // resolved guardrail rule set) rides along into the stream stage.
            extensions: self.extensions.clone(),
        }
    }

    /// Fold a finished `StreamContext` back in: usage lands in the execution
    /// result, stream-stage events are merged.
    pub fn absorb_stream(&mut self, stream: StreamContext) {
        if let Some(exec) = self.execution_result.as_mut() {
            if let Some(usage) = stream.final_usage {
                exec.result.usage = Some(usage);
            }
            exec.result.finish_reason = stream.finish_reason.clone();
        }
        self.first_token_timing = stream.first_token_timing;
        self.generation_duration_ms = stream.generation_duration_ms;
        self.events.merge_from(stream.events);
    }

    /// Borrow a `SettlementContext` for the Settlement stage. Moves the event
    /// bus out (so recorders can inspect events emitted by earlier stages);
    /// `absorb_settlement` moves it back.
    pub fn settlement_context(&mut self) -> SettlementContext {
        let target = self.serving_target();
        let exec = self.execution_result.as_ref();
        let model_id = exec
            .map(|execution| execution.model_id.clone())
            .or_else(|| target.as_ref().map(|target| target.service_id.clone()))
            .unwrap_or_default();
        let provider_id = exec
            .map(|execution| execution.provider_id.clone())
            .or_else(|| target.as_ref().map(|target| target.provider_name.clone()))
            .unwrap_or_default();
        let account_label = exec
            .and_then(|execution| execution.account_label.clone())
            .or_else(|| {
                target
                    .as_ref()
                    .and_then(|target| target.account_label.clone())
            });
        let usage = exec
            .and_then(|e| e.result.usage.clone())
            .unwrap_or_default();
        SettlementContext {
            request_id: self.request_id.clone(),
            caller: self.caller.clone(),
            target,
            model_id,
            provider_id,
            account_label,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            usage_origin: usage.origin,
            raw_usage: usage.raw.as_deref().cloned(),
            web_search_count: usage.web_search_count,
            media_input_count: count_media(self.prompt.messages.iter().flat_map(|m| &m.content)),
            // Note: for streamed requests both fields below are 0 / empty.
            // The streaming path only folds usage back (via `absorb_stream`);
            // response content and server_tool_calls are not reconstructed in
            // the IR. This is an intentional deferral, not a bug.
            media_output_count: count_media(
                exec.map(|e| e.result.content.as_slice())
                    .unwrap_or_default()
                    .iter(),
            ),
            server_tool_calls: exec
                .map(|e| e.server_tool_calls.clone())
                .unwrap_or_default(),
            streamed: false,
            request_duration_ms: self.request_duration_ms(),
            upstream_duration_ms: exec.and_then(|e| e.upstream_duration_ms),
            ttft_ms: self.first_token_timing.map(|timing| timing.ttft_ms),
            generation_duration_ms: self.generation_duration_ms,
            first_token_kind: self.first_token_timing.map(|timing| timing.kind),
            finish_reason: exec.and_then(|e| e.result.finish_reason.clone()),
            error: None,
            events: std::mem::take(&mut self.events),
        }
    }

    /// Fold a finished `SettlementContext` back in: the event bus (with any
    /// settlement-stage events) returns home.
    pub fn absorb_settlement(&mut self, settle: SettlementContext) {
        self.events = settle.events;
    }

    /// Render the final non-streaming HTTP response.
    pub fn into_response(self) -> PipelineResponse {
        let result = self.execution_result.map(|e| e.result).unwrap_or(
            crate::language_model::types::GenerateResult {
                content: Vec::new(),
                usage: None,
                finish_reason: None,
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            },
        );
        PipelineResponse {
            request_id: self.request_id,
            result,
        }
    }
}

fn prompt_content_chars(content: &Content) -> u64 {
    match content {
        Content::Text { text, .. } | Content::Reasoning { text, .. } => text.chars().count() as u64,
        Content::ToolCall {
            name, arguments, ..
        } => name
            .chars()
            .count()
            .saturating_add(arguments.chars().count()) as u64,
        Content::ToolResult {
            tool_name, output, ..
        } => tool_name
            .as_deref()
            .map_or(0u64, |name| name.chars().count() as u64)
            .saturating_add(output.to_provider_string().chars().count() as u64),
        other => serde_json::to_string(other)
            .map(|value| value.chars().count() as u64)
            .unwrap_or(0),
    }
}

/// The StreamHook-stage view, borrowed from `PipelineContext`. Carries the
/// mutable state that accrues while a stream is being consumed.
pub struct StreamContext {
    /// The request id.
    pub request_id: String,
    /// The caller.
    pub caller: CallerContext,
    /// The target actually serving the stream (chain head).
    pub target: Option<RoutingTarget>,
    /// Per-part usage accumulator.
    pub accumulated_usage: UsageAccumulator,
    /// Count of parts that entered the StreamHook stage.
    pub parts_emitted: u64,
    /// Usage finalised at `on_stream_end`, folded back into `PipelineContext`.
    pub final_usage: Option<Usage>,
    provider_started_at: Option<Instant>,
    first_token_timing: Option<FirstTokenTiming>,
    first_semantic_at: Option<Instant>,
    generation_duration_ms: Option<u64>,
    finish_reason: Option<FinishReason>,
    events: EventBus,
    metadata: HashMap<PluginId, serde_json::Value>,
    extensions: Extensions,
}

impl StreamContext {
    pub(crate) fn observe_upstream_part(&mut self, part: &StreamPart) {
        match part {
            StreamPart::Finish { reason } => {
                self.finish_reason = Some(reason.clone());
            }
            StreamPart::ResponseCompleted { status, .. } => {
                self.finish_reason = Some(match status.as_str() {
                    "completed" => FinishReason::Stop,
                    "incomplete" => FinishReason::Length,
                    other => FinishReason::Other(other.to_string()),
                });
            }
            _ => {}
        }

        let Some(provider_started_at) = self.provider_started_at else {
            return;
        };
        let Some(kind) = FirstTokenKind::from_part(part) else {
            return;
        };
        let observed_at = Instant::now();
        if let Some(first_semantic_at) = self.first_semantic_at {
            self.generation_duration_ms = Some(duration_millis(
                observed_at.duration_since(first_semantic_at),
            ));
        } else {
            self.first_semantic_at = Some(observed_at);
            self.first_token_timing = Some(FirstTokenTiming {
                ttft_ms: duration_millis(observed_at.duration_since(provider_started_at)),
                kind,
            });
            self.generation_duration_ms = Some(0);
        }
    }

    /// The first semantic output timing captured so far.
    pub fn first_token_timing(&self) -> Option<FirstTokenTiming> {
        self.first_token_timing
    }

    /// Time from the first to the most recent semantic stream delta.
    pub fn generation_duration_ms(&self) -> Option<u64> {
        self.generation_duration_ms
    }

    /// Canonical terminal reason observed from the upstream stream.
    pub fn finish_reason(&self) -> Option<&FinishReason> {
        self.finish_reason.as_ref()
    }

    /// Emit a typed event from within the StreamHook stage.
    pub fn emit<E: PipelineEvent>(&mut self, event: E) {
        self.events.emit(event);
    }

    /// Whether an event of type `E` was emitted in this stream context.
    pub fn has_event<E: PipelineEvent>(&self) -> bool {
        self.events.has::<E>()
    }

    /// The first emitted event of type `E`.
    pub fn get_event<E: PipelineEvent>(&self) -> Option<&E> {
        self.events.get::<E>()
    }

    /// Write this plugin's metadata blob.
    pub fn set_metadata(&mut self, plugin_id: &PluginId, value: serde_json::Value) {
        self.metadata.insert(plugin_id.clone(), value);
    }

    /// Read another plugin's metadata blob.
    pub fn get_metadata(&self, plugin_id: &PluginId) -> Option<&serde_json::Value> {
        self.metadata.get(plugin_id)
    }

    /// Read a typed, request-scoped extension propagated from the
    /// [`PipelineContext`] (see [`PipelineContext::insert_extension`]).
    pub fn extension<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.extensions.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PromptOverrides;
    use crate::language_model::stream::{StreamOutcome, StreamProcessor};
    use crate::language_model::types::StreamPart;
    use crate::language_model::{Message, PipelineRequest, Role};

    fn ctx_from_prompt(prompt: Prompt) -> PipelineContext {
        let req = PipelineRequest {
            request_id: "test".to_string(),
            model: prompt.model.clone(),
            caller: CallerContext::local(),
            headers: http::HeaderMap::new(),
            prompt,
            inbound_protocol: None,
        };
        PipelineContext::new(req)
    }

    fn empty_prompt() -> Prompt {
        Prompt {
            model: "gpt-5".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message {
                role: Role::User,
                content: vec![],
            }],
            tools: vec![],
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    #[test]
    fn apply_preset_overrides_sets_system_only_if_unset() {
        // No system in the request → preset fills it.
        let mut ctx = ctx_from_prompt(empty_prompt());
        let overrides = PromptOverrides {
            system_prompt: Some("Reason carefully.".to_string()),
            params: Default::default(),
        };
        ctx.apply_preset_overrides(&overrides);
        assert_eq!(ctx.prompt().system.as_deref(), Some("Reason carefully."));

        // Request already has a system → preset is a default, not an override.
        let mut prompt = empty_prompt();
        prompt.system = Some("Be concise.".to_string());
        let mut ctx = ctx_from_prompt(prompt);
        ctx.apply_preset_overrides(&overrides);
        assert_eq!(
            ctx.prompt().system.as_deref(),
            Some("Be concise."),
            "request-set system survives preset"
        );
    }

    #[test]
    fn apply_preset_overrides_merges_into_supplemental_params_without_clobbering() {
        let mut prompt = empty_prompt();
        prompt
            .params
            .extra
            .insert("temperature".to_string(), 0.7.into());
        let mut ctx = ctx_from_prompt(prompt);

        let mut overrides_map = serde_json::Map::new();
        overrides_map.insert("temperature".to_string(), 0.2.into()); // preset default
        overrides_map.insert("top_k".to_string(), 40.into()); // new key
        let overrides = PromptOverrides {
            system_prompt: None,
            params: overrides_map,
        };
        ctx.apply_preset_overrides(&overrides);

        // Existing key kept; new key filled in.
        assert_eq!(ctx.prompt().params.extra["temperature"], 0.7);
        assert_eq!(ctx.prompt().params.supplemental_extra["top_k"], 40);
    }

    #[test]
    fn apply_preset_overrides_promotes_normalized_protocol_fields() {
        let mut prompt = empty_prompt();
        prompt.params.store = Some(false);
        let mut ctx = ctx_from_prompt(prompt);

        let overrides = PromptOverrides {
            system_prompt: None,
            params: serde_json::Map::from_iter([
                ("store".to_string(), true.into()),
                ("parallel_tool_calls".to_string(), false.into()),
                (
                    "stream_options".to_string(),
                    serde_json::json!({"include_obfuscation": false}),
                ),
            ]),
        };
        ctx.apply_preset_overrides(&overrides);

        assert_eq!(ctx.prompt().params.store, Some(false));
        assert_eq!(ctx.prompt().params.parallel_tool_calls, Some(false));
        assert_eq!(
            ctx.prompt()
                .params
                .chat_stream_options
                .as_ref()
                .and_then(|options| options.include_obfuscation),
            Some(false)
        );
        assert!(ctx.prompt().params.supplemental_extra.is_empty());
    }

    #[test]
    fn empty_overrides_are_a_noop() {
        let mut ctx = ctx_from_prompt(empty_prompt());
        ctx.apply_preset_overrides(&PromptOverrides::default());
        assert!(ctx.prompt().system.is_none());
        assert!(ctx.prompt().params.extra.is_empty());
        assert!(ctx.prompt().params.supplemental_extra.is_empty());
    }

    #[test]
    fn inbound_protocol_threads_from_request_to_context() {
        // The HTTP server stamps the inbound protocol on the request; the
        // context exposes it to route resolution. `None` when unset.
        let mut req = PipelineRequest::new("m", CallerContext::local(), empty_prompt());
        assert_eq!(PipelineContext::new(req.clone()).inbound_protocol(), None);
        req.inbound_protocol = Some(ApiProtocol::Messages);
        assert_eq!(
            PipelineContext::new(req).inbound_protocol(),
            Some(ApiProtocol::Messages)
        );
    }

    fn prompt_with_text(system: Option<&str>, user_text: &str) -> Prompt {
        Prompt {
            model: "gpt-5".into(),
            system: system.map(str::to_string),
            system_provider_metadata: Default::default(),
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: user_text.to_string(),
                    provider_metadata: Default::default(),
                }],
            }],
            tools: vec![],
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: true,
        }
    }

    // Mirrors the private `UsageAccumulator::CHARS_PER_TOKEN_ESTIMATE`.
    const CHARS_PER_TOKEN: u64 = 4;

    #[test]
    fn stream_context_seeds_prompt_token_estimate() {
        let system = "You are a careful assistant.";
        let user = "Summarize the meeting notes in three concise bullet points.";
        let ctx = ctx_from_prompt(prompt_with_text(Some(system), user));

        let chars = (system.chars().count() + user.chars().count()) as u64;
        let sc = ctx.stream_context();
        assert_eq!(
            sc.accumulated_usage.estimated_prompt_tokens(),
            chars.div_ceil(CHARS_PER_TOKEN),
        );
        // No deltas observed yet → no output estimate.
        assert_eq!(sc.accumulated_usage.estimated_output_tokens(), 0);
    }

    #[test]
    fn prompt_token_estimate_counts_tool_call_payloads() {
        // Tool-call names and arguments are part of the model-visible prompt on
        // follow-up turns, so estimated usage should not collapse them to zero.
        let system = "sys";
        let tool_name = "get_weather";
        let arguments = "{\"city\":\"London\"}";
        let prompt = Prompt {
            model: "gpt-5".into(),
            system: Some(system.into()),
            system_provider_metadata: Default::default(),
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![Content::ToolCall {
                    id: "call_1".into(),
                    name: tool_name.into(),
                    arguments: arguments.into(),
                    provider_executed: false,
                    dynamic: false,
                    provider_metadata: Default::default(),
                }],
            }],
            tools: vec![],
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: true,
        };
        let ctx = ctx_from_prompt(prompt);
        let sc = ctx.stream_context();
        let expected_chars =
            system.chars().count() + tool_name.chars().count() + arguments.chars().count();
        assert_eq!(
            sc.accumulated_usage.estimated_prompt_tokens(),
            (expected_chars as u64).div_ceil(CHARS_PER_TOKEN),
        );
    }

    #[test]
    fn prompt_token_estimate_counts_tool_result_output() {
        let tool_name = "exec_command";
        let output = "12345678";
        let prompt = Prompt {
            model: "gpt-5".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message {
                role: Role::Tool,
                content: vec![Content::ToolResult {
                    call_id: "call_1".into(),
                    tool_name: Some(tool_name.into()),
                    output: crate::language_model::types::ToolResultOutput::Text {
                        value: output.into(),
                    },
                    dynamic: false,
                    provider_metadata: Default::default(),
                }],
            }],
            tools: vec![],
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: true,
        };
        let ctx = ctx_from_prompt(prompt);
        let sc = ctx.stream_context();
        let expected_chars = tool_name.chars().count() + output.chars().count();
        assert_eq!(
            sc.accumulated_usage.estimated_prompt_tokens(),
            (expected_chars as u64).div_ceil(CHARS_PER_TOKEN),
        );
    }

    #[tokio::test]
    async fn disconnect_bills_prompt_tokens_without_output() {
        let user = "Generate a long essay about distributed systems.";
        let ctx = ctx_from_prompt(prompt_with_text(None, user));
        let expected_prompt = (user.chars().count() as u64).div_ceil(CHARS_PER_TOKEN);

        // Client hangs up before any delta or usage frame arrives.
        let mut proc = StreamProcessor::new(vec![], vec![], ctx.stream_context());
        proc.finish(StreamOutcome::ClientDisconnected).await;

        let usage = proc
            .context()
            .final_usage
            .clone()
            .expect("disconnect with a non-empty prompt must still bill input tokens");
        assert_eq!(usage.prompt_tokens, expected_prompt);
        assert_eq!(usage.completion_tokens, 0);
        assert_eq!(
            usage.origin,
            crate::language_model::types::UsageOrigin::Estimated
        );
    }

    #[tokio::test]
    async fn disconnect_bills_prompt_and_estimated_output() {
        let user = "hi";
        let ctx = ctx_from_prompt(prompt_with_text(None, user));
        let mut proc = StreamProcessor::new(vec![], vec![], ctx.stream_context());

        // Drain some output, then disconnect before the trailing usage frame.
        let delta = "The answer is forty-two, and here is some more text.";
        proc.process_part(StreamPart::TextDelta { text: delta.into() })
            .await
            .expect("text delta passes through with no hooks");
        proc.finish(StreamOutcome::ClientDisconnected).await;

        let usage = proc
            .context()
            .final_usage
            .clone()
            .expect("billed on disconnect");
        assert_eq!(
            usage.prompt_tokens,
            (user.chars().count() as u64).div_ceil(CHARS_PER_TOKEN),
        );
        assert_eq!(
            usage.completion_tokens,
            (delta.chars().count() as u64).div_ceil(CHARS_PER_TOKEN),
        );
    }

    #[tokio::test]
    async fn completed_stream_without_usage_bills_prompt_and_estimated_output() {
        let user = "12345678";
        let ctx = ctx_from_prompt(prompt_with_text(None, user));
        let mut proc = StreamProcessor::new(vec![], vec![], ctx.stream_context());

        proc.process_part(StreamPart::TextDelta {
            text: "abcdefgh".into(),
        })
        .await
        .expect("text delta passes through with no hooks");
        proc.finish(StreamOutcome::Completed).await;

        let usage = proc
            .context()
            .final_usage
            .clone()
            .expect("completed stream without upstream usage should fall back to estimates");
        assert_eq!(usage.prompt_tokens, 2);
        assert_eq!(usage.completion_tokens, 2);
        assert_eq!(
            usage.origin,
            crate::language_model::types::UsageOrigin::Estimated
        );
    }

    #[test]
    fn settlement_context_preserves_usage_provenance_and_raw_payload() {
        let raw = serde_json::json!({
            "input_tokens": 12,
            "output_tokens": 4,
            "cache_read_input_tokens": 5
        });
        let mut ctx = ctx_from_prompt(prompt_with_text(None, "hello"));
        ctx.execution_result = Some(ExecutionResult {
            provider_id: "anthropic".into(),
            model_id: "claude".into(),
            account_label: None,
            result: crate::language_model::types::GenerateResult {
                content: Vec::new(),
                usage: Some(Usage {
                    prompt_tokens: 12,
                    completion_tokens: 4,
                    cache_read_tokens: 5,
                    origin: crate::language_model::types::UsageOrigin::ProviderReported,
                    raw: Some(Box::new(raw.clone())),
                    ..Default::default()
                }),
                finish_reason: None,
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            },
            request_duration_ms: 1,
            upstream_duration_ms: Some(1),
            server_tool_calls: Vec::new(),
        });

        let settlement = ctx.settlement_context();
        assert_eq!(
            settlement.usage_origin,
            crate::language_model::types::UsageOrigin::ProviderReported
        );
        assert_eq!(settlement.raw_usage.as_ref(), Some(&raw));
    }

    #[tokio::test]
    async fn completed_stream_with_zero_usage_bills_estimated_usage() {
        let user = "12345678";
        let ctx = ctx_from_prompt(prompt_with_text(None, user));
        let mut proc = StreamProcessor::new(vec![], vec![], ctx.stream_context());

        proc.process_part(StreamPart::TextDelta {
            text: "abcdefgh".into(),
        })
        .await
        .expect("text delta passes through with no hooks");
        proc.process_part(StreamPart::Usage {
            usage: Usage::default(),
        })
        .await
        .expect("zero usage frame passes through with no hooks");
        proc.finish(StreamOutcome::Completed).await;

        let usage = proc
            .context()
            .final_usage
            .clone()
            .expect("completed stream should have usage");
        assert_eq!(usage.prompt_tokens, 2);
        assert_eq!(usage.completion_tokens, 2);
    }

    #[tokio::test]
    async fn authoritative_usage_overrides_disconnect_estimate() {
        // A real upstream usage frame must win over the disconnect estimate,
        // even when the stream then ends as a client disconnect.
        let ctx = ctx_from_prompt(prompt_with_text(Some("system"), "user question"));
        let mut proc = StreamProcessor::new(vec![], vec![], ctx.stream_context());

        proc.process_part(StreamPart::Usage {
            usage: Usage {
                prompt_tokens: 11,
                completion_tokens: 22,
                ..Default::default()
            },
        })
        .await
        .unwrap();
        proc.finish(StreamOutcome::ClientDisconnected).await;

        let usage = proc
            .context()
            .final_usage
            .clone()
            .expect("authoritative usage billed");
        assert_eq!(
            usage.prompt_tokens, 11,
            "real prompt tokens, not the estimate"
        );
        assert_eq!(usage.completion_tokens, 22);
    }
}
