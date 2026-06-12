//! Pipeline contexts for the `language_model` protocol: `PipelineContext` (the
//! whole-request water-flow context) and `StreamContext` (the StreamHook-stage
//! view borrowed from it). `SettlementContext` lives in
//! [`crate::language_model::settlement`].

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::caller::CallerContext;
use crate::event::{EventBus, PipelineEvent};
use crate::language_model::settlement::SettlementContext;
use crate::language_model::stream::UsageAccumulator;
use crate::language_model::types::{
    ApiProtocol, Content, ExecutionResult, PipelineRequest, PipelineResponse, Prompt,
    RoutingTarget, Usage,
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

    // ===== accumulated: written per stage, readable downstream =====
    /// The resolved fallback chain (Stage 2).
    pub route_chain: Option<Vec<RoutingTarget>>,
    /// The execution result (Stage 3). Stored here rather than moved out so
    /// Settlement can borrow it without an ownership fight.
    pub execution_result: Option<ExecutionResult>,

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
            route_chain: None,
            execution_result: None,
            metadata: HashMap::new(),
            extensions: Extensions::default(),
            events: EventBus::new(),
            outbound_trace_headers: Mutex::new(None),
        }
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
    /// the prompt's `params.extra` so provider-specific knobs survive. Already-
    /// set request fields take precedence — a preset is a *default*, not an
    /// override of an explicit request value (except for `system` which has
    /// no merging surface).
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
            self.prompt
                .params
                .extra
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

    /// Rough `char` count of the request prompt's text — system instruction
    /// plus every message's text / reasoning content. Used to seed the
    /// [`UsageAccumulator`] so a client disconnect *before* the upstream usage
    /// frame can still estimate prompt tokens. Non-text parts (tool calls,
    /// tool results) are skipped; they carry no user-visible prose and the
    /// estimate only needs to beat `0` until the authoritative frame arrives.
    fn prompt_text_chars(&self) -> u64 {
        let prompt = self.prompt();
        let mut chars = prompt
            .system
            .as_ref()
            .map_or(0u64, |s| s.chars().count() as u64);
        for message in &prompt.messages {
            for content in &message.content {
                if let Content::Text { text, .. } | Content::Reasoning { text, .. } = content {
                    chars = chars.saturating_add(text.chars().count() as u64);
                }
            }
        }
        chars
    }

    /// Borrow a `StreamContext` for the StreamHook stage.
    pub fn stream_context(&self) -> StreamContext {
        let target = self.route_chain.as_ref().and_then(|c| c.first()).cloned();
        StreamContext {
            request_id: self.request_id.clone(),
            caller: self.caller.clone(),
            target,
            accumulated_usage: UsageAccumulator::with_prompt_chars(self.prompt_text_chars()),
            parts_emitted: 0,
            final_usage: None,
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
        if let (Some(exec), Some(usage)) = (self.execution_result.as_mut(), stream.final_usage) {
            exec.result.usage = Some(usage);
        }
        self.events.merge_from(stream.events);
    }

    /// Borrow a `SettlementContext` for the Settlement stage. Moves the event
    /// bus out (so recorders can inspect events emitted by earlier stages);
    /// `absorb_settlement` moves it back.
    pub fn settlement_context(&mut self) -> SettlementContext {
        let target = self.route_chain.as_ref().and_then(|c| c.first()).cloned();
        let exec = self.execution_result.as_ref();
        let usage = exec.and_then(|e| e.result.usage).unwrap_or_default();
        SettlementContext {
            request_id: self.request_id.clone(),
            caller: self.caller.clone(),
            target,
            model_id: exec.map(|e| e.model_id.clone()).unwrap_or_default(),
            provider_id: exec.map(|e| e.provider_id.clone()).unwrap_or_default(),
            account_label: exec.and_then(|e| e.account_label.clone()),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            streamed: false,
            latency_ms: exec.map(|e| e.latency_ms).unwrap_or(0),
            generation_time_ms: exec.map(|e| e.generation_time_ms).unwrap_or(0),
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
    events: EventBus,
    metadata: HashMap<PluginId, serde_json::Value>,
    extensions: Extensions,
}

impl StreamContext {
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
    fn apply_preset_overrides_merges_into_params_extra_without_clobbering() {
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
        assert_eq!(ctx.prompt().params.extra["top_k"], 40);
    }

    #[test]
    fn empty_overrides_are_a_noop() {
        let mut ctx = ctx_from_prompt(empty_prompt());
        ctx.apply_preset_overrides(&PromptOverrides::default());
        assert!(ctx.prompt().system.is_none());
        assert!(ctx.prompt().params.extra.is_empty());
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
    fn prompt_token_estimate_ignores_non_text_content() {
        // Only the system instruction is prose; the message's tool-call JSON
        // args must not inflate the prompt-token estimate.
        let system = "sys";
        let prompt = Prompt {
            model: "gpt-5".into(),
            system: Some(system.into()),
            system_provider_metadata: Default::default(),
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![Content::ToolCall {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    arguments: "{\"city\":\"a deliberately long value to be ignored\"}".into(),
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
        assert_eq!(
            sc.accumulated_usage.estimated_prompt_tokens(),
            (system.chars().count() as u64).div_ceil(CHARS_PER_TOKEN),
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
            .expect("disconnect with a non-empty prompt must still bill input tokens");
        assert_eq!(usage.prompt_tokens, expected_prompt);
        assert_eq!(usage.completion_tokens, 0);
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

        let usage = proc.context().final_usage.expect("billed on disconnect");
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
            .expect("authoritative usage billed");
        assert_eq!(
            usage.prompt_tokens, 11,
            "real prompt tokens, not the estimate"
        );
        assert_eq!(usage.completion_tokens, 22);
    }
}
