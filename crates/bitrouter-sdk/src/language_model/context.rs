//! Pipeline contexts for the `language_model` protocol: `PipelineContext` (the
//! whole-request water-flow context) and `StreamContext` (the StreamHook-stage
//! view borrowed from it). `SettlementContext` lives in
//! [`crate::language_model::settlement`].

use std::collections::HashMap;

use crate::caller::CallerContext;
use crate::event::{EventBus, PipelineEvent};
use crate::language_model::settlement::SettlementContext;
use crate::language_model::stream::UsageAccumulator;
use crate::language_model::types::{
    ExecutionResult, PipelineRequest, PipelineResponse, Prompt, RoutingTarget, Usage,
};
use crate::plugin::PluginId;

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

    // ===== accumulated: written per stage, readable downstream =====
    /// The resolved fallback chain (Stage 2).
    pub route_chain: Option<Vec<RoutingTarget>>,
    /// The execution result (Stage 3). Stored here rather than moved out so
    /// Settlement can borrow it without an ownership fight.
    pub execution_result: Option<ExecutionResult>,

    // ===== plugin extension data =====
    metadata: HashMap<PluginId, serde_json::Value>,

    // ===== typed event bus =====
    events: EventBus,
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
            route_chain: None,
            execution_result: None,
            metadata: HashMap::new(),
            events: EventBus::new(),
        }
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

    /// Borrow a `StreamContext` for the StreamHook stage.
    pub fn stream_context(&self) -> StreamContext {
        let target = self.route_chain.as_ref().and_then(|c| c.first()).cloned();
        StreamContext {
            request_id: self.request_id.clone(),
            caller: self.caller.clone(),
            target,
            accumulated_usage: UsageAccumulator::new(),
            parts_emitted: 0,
            final_usage: None,
            events: EventBus::new(),
            metadata: HashMap::new(),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PromptOverrides;
    use crate::language_model::{Message, PipelineRequest, Role};

    fn ctx_from_prompt(prompt: Prompt) -> PipelineContext {
        let req = PipelineRequest {
            request_id: "test".to_string(),
            model: prompt.model.clone(),
            caller: CallerContext::local(),
            headers: http::HeaderMap::new(),
            prompt,
        };
        PipelineContext::new(req)
    }

    fn empty_prompt() -> Prompt {
        Prompt {
            model: "gpt-5".into(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![],
            }],
            tools: vec![],
            params: Default::default(),
            response_format: None,
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
}
