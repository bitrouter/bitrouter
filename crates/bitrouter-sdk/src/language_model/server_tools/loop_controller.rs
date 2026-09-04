//! [`ServerToolLoop`] — the non-streaming controller. It injects router tools
//! into a working prompt, then drives upstream turns through a caller-supplied
//! [`UpstreamTurn`], executing router-owned tool calls and looping until the
//! model stops calling them or a bound is hit.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use super::approval::ApprovalPolicy;
use super::classify::{RouterCall, TurnDisposition, classify_turn};
use super::config::ServerToolLoopConfig;
use super::toolset::{ToolContext, ToolsetRegistry};
use crate::error::{BitrouterError, Result};
use crate::language_model::types::{
    Content, ExecutionResult, FinishReason, Message, Prompt, ProviderMetadata, Role,
    ServerToolCall, ServerToolKind, ServerToolStatus, Tool, ToolResultOutput, Usage, UsageOrigin,
};

/// One upstream turn for a working prompt — the loop's callback into the
/// pipeline's `execute_with_fallback`. A trait (rather than a closure) so the
/// resulting future has a concrete, `Send` type when the pipeline spawns the
/// request.
#[async_trait]
pub trait UpstreamTurn: Send + Sync {
    /// Run one upstream turn for `prompt`.
    async fn run(&self, prompt: &Prompt) -> Result<ExecutionResult>;

    /// Whether router-owned continuation work may begin. Implementations that
    /// do not have request-lifetime control remain continuable by default.
    fn should_continue(&self) -> bool {
        true
    }
}

/// Drives the server-side tool loop over a [`ToolsetRegistry`].
pub struct ServerToolLoop {
    registry: ToolsetRegistry,
    config: ServerToolLoopConfig,
    approval: Arc<dyn ApprovalPolicy>,
}

/// Pipeline-internal provenance for the response exposed by a non-streaming
/// server-tool loop. The public loop API remains the historical
/// `ExecutionResult`; the pipeline additionally needs to know whether that
/// result is a provider terminal or a router-synthetic truncation.
pub(crate) struct ServerToolLoopOutcome {
    pub(crate) result: ExecutionResult,
    pub(crate) provider_terminal_exposed: bool,
}

enum ToolBatchOutcome {
    Completed {
        results: Vec<Content>,
        had_error: bool,
    },
    Interrupted {
        results: Vec<Content>,
        had_error: bool,
    },
}

impl ServerToolLoop {
    /// Build a loop over `registry`, bounded by `config`, gated by `approval`.
    pub fn new(
        registry: ToolsetRegistry,
        config: ServerToolLoopConfig,
        approval: Arc<dyn ApprovalPolicy>,
    ) -> Self {
        Self {
            registry,
            config,
            approval,
        }
    }

    /// The loop's bounds.
    pub fn config(&self) -> &ServerToolLoopConfig {
        &self.config
    }

    /// The MCP server backing the toolset that owns `name`, if any — used to
    /// label a streamed router tool call for `mcp_tool_use` rendering.
    pub(crate) fn server_name_for(&self, name: &str) -> Option<String> {
        self.registry
            .resolve(name)
            .and_then(|set| set.server_name())
            .map(str::to_string)
    }

    /// Clone `base`, advertise the registry's router tools on it (failing on a
    /// name collision with a caller tool), and return the working prompt plus
    /// the set of router-owned tool names.
    ///
    /// A caller may *declare* a router tool as a provider-defined tool (e.g. an
    /// `{type: "<provider>:<tool>"}` server-tool entry). Such declarations are
    /// dropped from the working prompt for any tool the registry is actually
    /// advertising this request: the toolset re-advertises that tool as an
    /// executable function tool, and the raw provider-defined form must not reach
    /// the upstream — it has no portable wire form and a same-protocol upstream
    /// would reject an unknown server tool. Dropping them also avoids a spurious
    /// self-collision below.
    ///
    /// The strip predicate is membership in the advertised set (`owned`), matched
    /// by the declaration's tail name: a toolset may own a declaration under a
    /// namespaced name (e.g. `bitrouter:fusion`) while advertising the bare
    /// executable (`fusion`), and the namespaced declaration must still be
    /// stripped. Crucially, a provider-defined tool the registry *could* own but
    /// is **not** advertising this request (e.g. a caller's native `web_search`
    /// when `bitrouter:web_search` was not declared) is left untouched, so the
    /// caller never silently loses a genuine provider tool.
    pub(crate) async fn inject(
        &self,
        base: &Prompt,
        ctx: &ToolContext,
    ) -> Result<(Prompt, std::collections::BTreeSet<String>)> {
        let (injected, owned) = self.registry.list_all(ctx).await?;
        let mut working = base.clone();
        working.tools.retain(|t| {
            !(matches!(t, Tool::ProviderDefined { .. }) && owned.contains(tool_tail(t.name())))
        });
        for tool in &injected {
            if working.tools.iter().any(|t| t.name() == tool.name()) {
                return Err(BitrouterError::Internal(format!(
                    "router tool name collides with a caller tool: {}",
                    tool.name()
                )));
            }
        }
        working.tools.extend(injected);
        Ok((working, owned))
    }

    /// Run the loop. `upstream` performs one upstream turn (with fallback) for a
    /// working prompt; the loop owns the working prompt, injects router tools
    /// into it, appends assistant + tool-result turns between iterations, and
    /// accumulates usage across iterations.
    ///
    /// Returns the final upstream [`ExecutionResult`] (its `usage` replaced by
    /// the accumulated total). On reaching a bound, the result carries a
    /// truncation finish reason.
    pub async fn run(
        &self,
        base: &Prompt,
        ctx: &ToolContext,
        upstream: &dyn UpstreamTurn,
    ) -> Result<ExecutionResult> {
        Ok(self.run_with_provenance(base, ctx, upstream).await?.result)
    }

    pub(crate) async fn run_with_provenance(
        &self,
        base: &Prompt,
        ctx: &ToolContext,
        upstream: &dyn UpstreamTurn,
    ) -> Result<ServerToolLoopOutcome> {
        let (mut working, owned) = self.inject(base, ctx).await?;

        let mut total = Usage::default();
        let mut had_usage = false;
        let start = Instant::now();
        let mut consecutive_errors = 0u32;
        let mut rounds = 0u32;
        let mut server_calls: Vec<ServerToolCall> = Vec::new();
        let mut previous_result: Option<ExecutionResult> = None;

        loop {
            if !upstream.should_continue()
                && let Some(mut result) = previous_result.take()
            {
                result.server_tool_calls = std::mem::take(&mut server_calls);
                return Ok(ServerToolLoopOutcome {
                    result: truncate(result, total, had_usage, "client_disconnected"),
                    provider_terminal_exposed: false,
                });
            }
            let mut result = match upstream.run(&working).await {
                Ok(result) => result,
                Err(BitrouterError::ClientDisconnected) => {
                    if let Some(mut result) = previous_result.take() {
                        result.server_tool_calls = std::mem::take(&mut server_calls);
                        return Ok(ServerToolLoopOutcome {
                            result: truncate(result, total, had_usage, "client_disconnected"),
                            provider_terminal_exposed: false,
                        });
                    }
                    return Err(BitrouterError::ClientDisconnected);
                }
                Err(error) => return Err(error),
            };
            if let Some(usage) = &result.result.usage {
                add_usage(&mut total, usage);
                had_usage = true;
            }

            match classify_turn(&result.result.content, &owned) {
                TurnDisposition::Done | TurnDisposition::HandBack => {
                    if had_usage {
                        result.result.usage = Some(total);
                    }
                    record_provider_calls(&mut server_calls, &result.result.content);
                    result.server_tool_calls = std::mem::take(&mut server_calls);
                    return Ok(ServerToolLoopOutcome {
                        result,
                        provider_terminal_exposed: true,
                    });
                }
                TurnDisposition::Execute(calls) => {
                    if !upstream.should_continue() {
                        result.server_tool_calls = std::mem::take(&mut server_calls);
                        return Ok(ServerToolLoopOutcome {
                            result: truncate(result, total, had_usage, "client_disconnected"),
                            provider_terminal_exposed: false,
                        });
                    }
                    if rounds >= self.config.max_iterations
                        || start.elapsed() >= self.config.total_budget
                    {
                        result.server_tool_calls = std::mem::take(&mut server_calls);
                        return Ok(ServerToolLoopOutcome {
                            result: truncate(result, total, had_usage, "max_tool_iterations"),
                            provider_terminal_exposed: false,
                        });
                    }
                    record_provider_calls(&mut server_calls, &result.result.content);
                    let (tool_results, had_error) = match self
                        .execute_calls(&calls, ctx, upstream)
                        .await
                    {
                        ToolBatchOutcome::Completed { results, had_error } => {
                            record_router_calls(
                                &mut server_calls,
                                &calls,
                                results.len(),
                                had_error,
                            );
                            (results, had_error)
                        }
                        ToolBatchOutcome::Interrupted { results, had_error } => {
                            record_router_calls(
                                &mut server_calls,
                                &calls,
                                results.len(),
                                had_error,
                            );
                            result.server_tool_calls = std::mem::take(&mut server_calls);
                            return Ok(ServerToolLoopOutcome {
                                result: truncate(result, total, had_usage, "client_disconnected"),
                                provider_terminal_exposed: false,
                            });
                        }
                    };
                    if !upstream.should_continue() {
                        result.server_tool_calls = std::mem::take(&mut server_calls);
                        return Ok(ServerToolLoopOutcome {
                            result: truncate(result, total, had_usage, "client_disconnected"),
                            provider_terminal_exposed: false,
                        });
                    }
                    consecutive_errors = if had_error { consecutive_errors + 1 } else { 0 };
                    append_turn(&mut working, result.result.content.clone(), tool_results);
                    rounds += 1;
                    if consecutive_errors >= self.config.max_consecutive_errors {
                        result.server_tool_calls = std::mem::take(&mut server_calls);
                        return Ok(ServerToolLoopOutcome {
                            result: truncate(result, total, had_usage, "tool_errors"),
                            provider_terminal_exposed: false,
                        });
                    }
                    previous_result = Some(result);
                }
            }
        }
    }

    /// Execute one router-owned call: approval gate, then the owning toolset
    /// under the per-tool timeout. Returns the result output and whether it
    /// errored. Shared by the non-streaming loop and the stream stitcher.
    pub(crate) async fn call_one(
        &self,
        call: &RouterCall,
        ctx: &ToolContext,
    ) -> (ToolResultOutput, bool) {
        let approved = self.approval.allow(call, ctx.caller()).await;
        self.execute_approved_call(call, ctx, approved).await
    }

    async fn execute_approved_call(
        &self,
        call: &RouterCall,
        ctx: &ToolContext,
        approved: bool,
    ) -> (ToolResultOutput, bool) {
        if !approved {
            return (
                ToolResultOutput::ExecutionDenied {
                    reason: Some("denied by approval policy".to_string()),
                },
                false,
            );
        }
        let Some(set) = self.registry.resolve(&call.name) else {
            return (
                ToolResultOutput::ErrorText {
                    value: format!("no toolset owns '{}'", call.name),
                },
                true,
            );
        };
        match tokio::time::timeout(
            self.config.tool_timeout,
            set.call_tool(&call.name, &call.arguments, ctx),
        )
        .await
        {
            Ok(Ok(out)) => (out, false),
            Ok(Err(err)) => (
                ToolResultOutput::ErrorText {
                    value: err.to_string(),
                },
                true,
            ),
            Err(_) => (
                ToolResultOutput::ErrorText {
                    value: format!("tool '{}' timed out", call.name),
                },
                true,
            ),
        }
    }

    /// Execute each router-owned call without cancelling one that already
    /// started. The interrupted form retains results for completed calls while
    /// preventing approval or side effects for every later call.
    async fn execute_calls(
        &self,
        calls: &[RouterCall],
        ctx: &ToolContext,
        upstream: &dyn UpstreamTurn,
    ) -> ToolBatchOutcome {
        let mut results = Vec::with_capacity(calls.len());
        let mut had_error = false;
        for call in calls {
            if !upstream.should_continue() {
                return ToolBatchOutcome::Interrupted { results, had_error };
            }
            let approved = self.approval.allow(call, ctx.caller()).await;
            if !upstream.should_continue() {
                return ToolBatchOutcome::Interrupted { results, had_error };
            }
            let (output, err) = self.execute_approved_call(call, ctx, approved).await;
            had_error |= err;
            results.push(Content::ToolResult {
                call_id: call.id.clone(),
                tool_name: Some(call.name.clone()),
                output,
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            });
            if !upstream.should_continue() {
                return ToolBatchOutcome::Interrupted { results, had_error };
            }
        }
        ToolBatchOutcome::Completed { results, had_error }
    }
}

/// The bare tail of a (possibly namespaced) tool name: the final `:`/`.`
/// segment. Matches a `bitrouter:fusion` declaration against the advertised
/// bare `fusion`, while leaving an unrelated provider tail (e.g.
/// `web_search_20250305`) distinct from `web_search`.
fn tool_tail(name: &str) -> &str {
    name.rsplit([':', '.']).next().unwrap_or(name)
}

/// Append the model's tool-call turn and the tool-result turn to the working
/// prompt so the next upstream call sees the results.
fn append_turn(working: &mut Prompt, assistant_content: Vec<Content>, tool_results: Vec<Content>) {
    working.messages.push(Message {
        role: Role::Assistant,
        content: assistant_content,
    });
    working.messages.push(Message {
        role: Role::Tool,
        content: tool_results,
    });
}

/// Sum the per-iteration usage into the running total.
pub(crate) fn add_usage(total: &mut Usage, add: &Usage) {
    let total_was_empty = total.prompt_tokens == 0
        && total.completion_tokens == 0
        && total.reasoning_tokens == 0
        && total.cache_read_tokens == 0
        && total.cache_write_tokens == 0
        && total.web_search_count == 0
        && total.origin == UsageOrigin::Unknown
        && total.raw.is_none();
    total.prompt_tokens = total.prompt_tokens.saturating_add(add.prompt_tokens);
    total.completion_tokens = total
        .completion_tokens
        .saturating_add(add.completion_tokens);
    total.reasoning_tokens = total.reasoning_tokens.saturating_add(add.reasoning_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(add.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(add.cache_write_tokens);
    total.web_search_count = total.web_search_count.saturating_add(add.web_search_count);
    total.origin = if total_was_empty {
        add.origin
    } else {
        combined_usage_origin(total.origin, add.origin)
    };
    if total_was_empty {
        total.raw = add.raw.clone();
    } else if let Some(raw) = add.raw.as_deref() {
        let mut fragments = match total.raw.take() {
            Some(existing) => existing
                .get("provider_usage_fragments")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_else(|| vec![*existing]),
            None => Vec::new(),
        };
        fragments.push(raw.clone());
        total.raw = Some(Box::new(serde_json::json!({
            "provider_usage_fragments": fragments
        })));
    }
}

fn combined_usage_origin(left: UsageOrigin, right: UsageOrigin) -> UsageOrigin {
    match (left, right) {
        (UsageOrigin::Unknown, _) | (_, UsageOrigin::Unknown) => UsageOrigin::Unknown,
        (UsageOrigin::Estimated, _) | (_, UsageOrigin::Estimated) => UsageOrigin::Estimated,
        (UsageOrigin::ProviderReported, UsageOrigin::ProviderReported) => {
            UsageOrigin::ProviderReported
        }
        (UsageOrigin::AuthoritativeReceipt, UsageOrigin::AuthoritativeReceipt) => {
            UsageOrigin::AuthoritativeReceipt
        }
        _ => UsageOrigin::Unknown,
    }
}

/// Record provider-executed tool calls found in a turn's content.
fn record_provider_calls(out: &mut Vec<ServerToolCall>, content: &[Content]) {
    for c in content {
        if let Content::ToolCall {
            name,
            id,
            provider_executed: true,
            ..
        } = c
        {
            out.push(ServerToolCall {
                name: name.clone(),
                kind: ServerToolKind::Provider,
                call_id: Some(id.clone()),
                status: ServerToolStatus::Ok,
                // result_count is populated in a later stage; provider result
                // extraction is not wired here yet.
                result_count: 0,
            });
        }
    }
}

/// Record only router calls whose execution produced a result before a batch
/// interruption. Status intentionally retains the historical per-turn
/// aggregate error semantics.
fn record_router_calls(
    out: &mut Vec<ServerToolCall>,
    calls: &[RouterCall],
    completed: usize,
    had_error: bool,
) {
    for call in calls.iter().take(completed) {
        out.push(ServerToolCall {
            name: call.name.clone(),
            kind: ServerToolKind::Router,
            call_id: Some(call.id.clone()),
            status: if had_error {
                ServerToolStatus::Error
            } else {
                ServerToolStatus::Ok
            },
            result_count: 0,
        });
    }
}

/// Finish a bounded loop: replace usage with the accumulated total and set a
/// truncation finish reason.
fn truncate(
    mut result: ExecutionResult,
    total: Usage,
    had_usage: bool,
    reason: &str,
) -> ExecutionResult {
    if had_usage {
        result.result.usage = Some(total);
    }
    result.result.finish_reason = Some(FinishReason::Other(reason.to_string()));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::server_tools::approval::{AllowAll, ApprovalPolicy};
    use crate::language_model::server_tools::toolset::RouterToolset;
    use crate::language_model::types::{GenerateResult, Tool};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard};
    use tokio::sync::{Notify, Semaphore};

    #[test]
    fn aggregated_usage_retains_provenance_and_every_raw_fragment() {
        let first_raw = serde_json::json!({"input_tokens": 10, "output_tokens": 2});
        let second_raw = serde_json::json!({"input_tokens": 20, "output_tokens": 3});
        let mut total = Usage::default();

        add_usage(
            &mut total,
            &Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                web_search_count: 1,
                origin: crate::language_model::types::UsageOrigin::ProviderReported,
                raw: Some(Box::new(first_raw.clone())),
                ..Default::default()
            },
        );
        add_usage(
            &mut total,
            &Usage {
                prompt_tokens: 20,
                completion_tokens: 3,
                cache_read_tokens: 5,
                web_search_count: 2,
                origin: crate::language_model::types::UsageOrigin::ProviderReported,
                raw: Some(Box::new(second_raw.clone())),
                ..Default::default()
            },
        );

        assert_eq!(total.prompt_tokens, 30);
        assert_eq!(total.completion_tokens, 5);
        assert_eq!(total.cache_read_tokens, 5);
        assert_eq!(total.web_search_count, 3);
        assert_eq!(
            total.origin,
            crate::language_model::types::UsageOrigin::ProviderReported
        );
        assert_eq!(
            total.raw.as_deref(),
            Some(&serde_json::json!({"provider_usage_fragments": [first_raw, second_raw]}))
        );
    }

    #[test]
    fn estimated_fragment_conservatively_marks_aggregate_estimated() {
        let mut total = Usage::default();
        add_usage(
            &mut total,
            &Usage {
                prompt_tokens: 1,
                origin: crate::language_model::types::UsageOrigin::ProviderReported,
                raw: Some(Box::new(serde_json::json!({"input_tokens": 1}))),
                ..Default::default()
            },
        );
        add_usage(
            &mut total,
            &Usage {
                completion_tokens: 1,
                origin: crate::language_model::types::UsageOrigin::Estimated,
                ..Default::default()
            },
        );

        assert_eq!(
            total.origin,
            crate::language_model::types::UsageOrigin::Estimated
        );
    }

    struct MockToolset {
        names: Vec<String>,
        fail: bool,
    }

    #[async_trait]
    impl RouterToolset for MockToolset {
        async fn list_tools(&self, _ctx: &ToolContext) -> Result<Vec<Tool>> {
            Ok(self
                .names
                .iter()
                .map(|n| Tool::Function {
                    name: n.clone(),
                    description: None,
                    parameters: serde_json::json!({ "type": "object" }),
                    strict: None,
                    provider_metadata: ProviderMetadata::new(),
                })
                .collect())
        }
        async fn call_tool(
            &self,
            name: &str,
            _arguments: &str,
            _ctx: &ToolContext,
        ) -> Result<ToolResultOutput> {
            if self.fail {
                Err(BitrouterError::Internal(format!("{name} boom")))
            } else {
                Ok(ToolResultOutput::Text {
                    value: format!("ran {name}"),
                })
            }
        }
        fn owns(&self, name: &str) -> bool {
            self.names.iter().any(|n| n == name)
        }
    }

    /// Replays canned upstream results, recording each working prompt it saw.
    struct ScriptedUpstream {
        responses: Mutex<VecDeque<ExecutionResult>>,
        seen: Mutex<Vec<Prompt>>,
    }

    impl ScriptedUpstream {
        fn new(responses: Vec<ExecutionResult>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                seen: Mutex::new(Vec::new()),
            }
        }
        fn seen(&self) -> Vec<Prompt> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl UpstreamTurn for ScriptedUpstream {
        async fn run(&self, prompt: &Prompt) -> Result<ExecutionResult> {
            self.seen.lock().unwrap().push(prompt.clone());
            Ok(self.responses.lock().unwrap().pop_front().unwrap())
        }
    }

    /// Replays canned turns while exposing an independently controlled
    /// continuation signal. A turn may simulate the client disconnecting as
    /// soon as its provider response has been received.
    struct DisconnectUpstream {
        responses: Mutex<VecDeque<ExecutionResult>>,
        calls: AtomicUsize,
        continue_: AtomicBool,
        disconnect_after_run: bool,
    }

    impl DisconnectUpstream {
        fn new(responses: Vec<ExecutionResult>, disconnect_after_run: bool) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: AtomicUsize::new(0),
                continue_: AtomicBool::new(true),
                disconnect_after_run,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn disconnect(&self) {
            self.continue_.store(false, Ordering::SeqCst);
        }

        fn should_continue(&self) -> bool {
            self.continue_.load(Ordering::SeqCst)
        }

        fn responses(&self) -> MutexGuard<'_, VecDeque<ExecutionResult>> {
            match self.responses.lock() {
                Ok(responses) => responses,
                Err(poisoned) => poisoned.into_inner(),
            }
        }
    }

    #[async_trait]
    impl UpstreamTurn for DisconnectUpstream {
        async fn run(&self, _prompt: &Prompt) -> Result<ExecutionResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let result = self.responses().pop_front().ok_or_else(|| {
                BitrouterError::Internal("disconnect test exhausted upstream turns".to_string())
            })?;
            if self.disconnect_after_run {
                self.disconnect();
            }
            Ok(result)
        }

        fn should_continue(&self) -> bool {
            DisconnectUpstream::should_continue(self)
        }
    }

    struct LaterDisconnectUpstream {
        responses: Mutex<VecDeque<Result<ExecutionResult>>>,
        calls: AtomicUsize,
    }

    impl LaterDisconnectUpstream {
        fn new(responses: Vec<Result<ExecutionResult>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl UpstreamTurn for LaterDisconnectUpstream {
        async fn run(&self, _prompt: &Prompt) -> Result<ExecutionResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let next = match self.responses.lock() {
                Ok(mut responses) => responses.pop_front(),
                Err(poisoned) => poisoned.into_inner().pop_front(),
            };
            next.unwrap_or_else(|| {
                Err(BitrouterError::Internal(
                    "later-disconnect test exhausted upstream turns".to_string(),
                ))
            })
        }
    }

    struct CountingToolset {
        calls: AtomicUsize,
        finished: AtomicUsize,
        started: Option<Arc<Notify>>,
        release: Option<Arc<Semaphore>>,
        fail: bool,
    }

    struct GatedApproval {
        started: Arc<Notify>,
        release: Arc<Semaphore>,
    }

    #[async_trait]
    impl ApprovalPolicy for GatedApproval {
        async fn allow(&self, _call: &RouterCall, _caller: &CallerContext) -> bool {
            self.started.notify_one();
            match self.release.acquire().await {
                Ok(permit) => {
                    permit.forget();
                    true
                }
                Err(_) => false,
            }
        }
    }

    impl CountingToolset {
        fn immediate() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                finished: AtomicUsize::new(0),
                started: None,
                release: None,
                fail: false,
            }
        }

        fn gated(started: Arc<Notify>, release: Arc<Semaphore>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                finished: AtomicUsize::new(0),
                started: Some(started),
                release: Some(release),
                fail: false,
            }
        }

        fn gated_failing(started: Arc<Notify>, release: Arc<Semaphore>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                finished: AtomicUsize::new(0),
                started: Some(started),
                release: Some(release),
                fail: true,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn finished(&self) -> usize {
            self.finished.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl RouterToolset for CountingToolset {
        async fn list_tools(&self, _ctx: &ToolContext) -> Result<Vec<Tool>> {
            Ok(vec![Tool::Function {
                name: "search".to_string(),
                description: None,
                parameters: serde_json::json!({ "type": "object" }),
                strict: None,
                provider_metadata: ProviderMetadata::new(),
            }])
        }

        async fn call_tool(
            &self,
            _name: &str,
            _arguments: &str,
            _ctx: &ToolContext,
        ) -> Result<ToolResultOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(started) = &self.started {
                started.notify_one();
            }
            if let Some(release) = &self.release {
                let permit = release.acquire().await.map_err(|_| {
                    BitrouterError::Internal("disconnect test tool gate closed".to_string())
                })?;
                permit.forget();
            }
            self.finished.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(BitrouterError::Internal(
                    "disconnect test tool failed".to_string(),
                ))
            } else {
                Ok(ToolResultOutput::Text {
                    value: "ran search".to_string(),
                })
            }
        }

        fn owns(&self, name: &str) -> bool {
            name == "search"
        }
    }

    fn disconnect_loop_with(toolset: Arc<CountingToolset>) -> ServerToolLoop {
        disconnect_loop_with_approval(toolset, Arc::new(AllowAll))
    }

    fn disconnect_loop_with_approval(
        toolset: Arc<CountingToolset>,
        approval: Arc<dyn ApprovalPolicy>,
    ) -> ServerToolLoop {
        ServerToolLoop::new(
            ToolsetRegistry::new(vec![toolset]),
            ServerToolLoopConfig::default(),
            approval,
        )
    }

    fn loop_with(names: &[&str], fail: bool, config: ServerToolLoopConfig) -> ServerToolLoop {
        let toolset = Arc::new(MockToolset {
            names: names.iter().map(|s| s.to_string()).collect(),
            fail,
        });
        ServerToolLoop::new(
            ToolsetRegistry::new(vec![toolset]),
            config,
            Arc::new(AllowAll),
        )
    }

    fn tool_ctx() -> ToolContext {
        ToolContext::new(CallerContext::local(), Default::default())
    }

    fn base_prompt() -> Prompt {
        Prompt {
            model: "m".to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: vec![Message::text(Role::User, "hi")],
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn exec(content: Vec<Content>) -> ExecutionResult {
        ExecutionResult {
            provider_id: "p".to_string(),
            model_id: "m".to_string(),
            account_label: None,
            result: GenerateResult {
                content,
                usage: Some(Usage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    reasoning_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    web_search_count: 0,
                    ..Default::default()
                }),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_details: None,
                provider_metadata: ProviderMetadata::new(),
            },
            request_duration_ms: 0,
            upstream_duration_ms: None,
            server_tool_calls: Vec::new(),
        }
    }

    fn tool_call(name: &str) -> Content {
        tool_call_with_id(name, &format!("{name}-1"))
    }

    fn tool_call_with_id(name: &str, id: &str) -> Content {
        Content::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
            provider_executed: false,
            dynamic: false,
            provider_metadata: ProviderMetadata::new(),
        }
    }

    fn text(s: &str) -> Content {
        Content::Text {
            text: s.to_string(),
            provider_metadata: ProviderMetadata::new(),
        }
    }

    fn truncation_reason(result: &ExecutionResult) -> Option<&str> {
        match result.result.finish_reason.as_ref() {
            Some(FinishReason::Other(reason)) => Some(reason.as_str()),
            _ => None,
        }
    }

    #[tokio::test]
    async fn server_tool_disconnect_after_provider_result_preserves_usage_without_running_tool()
    -> Result<()> {
        let expected_usage = Usage {
            prompt_tokens: 13,
            completion_tokens: 5,
            cache_read_tokens: 3,
            origin: UsageOrigin::ProviderReported,
            raw: Some(Box::new(serde_json::json!({
                "input_tokens": 13,
                "output_tokens": 5
            }))),
            ..Default::default()
        };
        let mut provider_result = exec(vec![tool_call("search")]);
        provider_result.result.usage = Some(expected_usage.clone());
        let upstream = DisconnectUpstream::new(
            vec![provider_result, exec(vec![text("unexpected later turn")])],
            true,
        );
        let toolset = Arc::new(CountingToolset::immediate());
        let loop_ = disconnect_loop_with(Arc::clone(&toolset));

        let outcome = loop_
            .run_with_provenance(&base_prompt(), &tool_ctx(), &upstream)
            .await?;

        assert!(!upstream.should_continue());
        assert_eq!(upstream.calls(), 1);
        assert_eq!(toolset.calls(), 0);
        assert_eq!(outcome.result.result.usage, Some(expected_usage));
        assert!(!outcome.provider_terminal_exposed);
        assert_eq!(
            truncation_reason(&outcome.result),
            Some("client_disconnected")
        );
        Ok(())
    }

    #[tokio::test]
    async fn server_tool_disconnect_during_running_tool_finishes_tool_without_next_upstream_turn()
    -> Result<()> {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let toolset = Arc::new(CountingToolset::gated(
            Arc::clone(&started),
            Arc::clone(&release),
        ));
        let loop_ = disconnect_loop_with(Arc::clone(&toolset));
        let upstream = Arc::new(DisconnectUpstream::new(
            vec![
                exec(vec![tool_call("search")]),
                exec(vec![text("unexpected later turn")]),
            ],
            false,
        ));
        let task_upstream = Arc::clone(&upstream);

        let task = tokio::spawn(async move {
            loop_
                .run_with_provenance(&base_prompt(), &tool_ctx(), task_upstream.as_ref())
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), started.notified())
            .await
            .map_err(|_| {
                BitrouterError::Internal(
                    "timed out waiting for disconnect test tool to start".to_string(),
                )
            })?;
        upstream.disconnect();
        release.add_permits(1);
        let outcome = task.await.map_err(|error| {
            BitrouterError::Internal(format!("disconnect test task failed: {error}"))
        })??;

        assert_eq!(toolset.calls(), 1);
        assert_eq!(toolset.finished(), 1);
        assert_eq!(upstream.calls(), 1);
        assert!(!outcome.provider_terminal_exposed);
        assert_eq!(
            truncation_reason(&outcome.result),
            Some("client_disconnected")
        );
        Ok(())
    }

    #[tokio::test]
    async fn server_tool_disconnect_during_approval_starts_no_tool_side_effect() -> Result<()> {
        let approval_started = Arc::new(Notify::new());
        let approval_release = Arc::new(Semaphore::new(0));
        let toolset = Arc::new(CountingToolset::immediate());
        let loop_ = disconnect_loop_with_approval(
            Arc::clone(&toolset),
            Arc::new(GatedApproval {
                started: Arc::clone(&approval_started),
                release: Arc::clone(&approval_release),
            }),
        );
        let upstream = Arc::new(DisconnectUpstream::new(
            vec![
                exec(vec![tool_call("search")]),
                exec(vec![text("unexpected later turn")]),
            ],
            false,
        ));
        let task_upstream = Arc::clone(&upstream);

        let task = tokio::spawn(async move {
            loop_
                .run_with_provenance(&base_prompt(), &tool_ctx(), task_upstream.as_ref())
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            approval_started.notified(),
        )
        .await
        .map_err(|_| {
            BitrouterError::Internal("timed out waiting for disconnect test approval".to_string())
        })?;
        upstream.disconnect();
        approval_release.add_permits(1);
        let outcome = task.await.map_err(|error| {
            BitrouterError::Internal(format!("disconnect approval test task failed: {error}"))
        })??;

        assert_eq!(upstream.calls(), 1);
        assert_eq!(toolset.calls(), 0);
        assert!(outcome.result.server_tool_calls.is_empty());
        assert!(!outcome.provider_terminal_exposed);
        assert_eq!(
            truncation_reason(&outcome.result),
            Some("client_disconnected")
        );
        Ok(())
    }

    #[tokio::test]
    async fn server_tool_disconnect_after_first_batch_call_starts_no_later_call() -> Result<()> {
        let first_started = Arc::new(Notify::new());
        let first_release = Arc::new(Semaphore::new(0));
        let toolset = Arc::new(CountingToolset::gated_failing(
            Arc::clone(&first_started),
            Arc::clone(&first_release),
        ));
        let loop_ = ServerToolLoop::new(
            ToolsetRegistry::new(vec![toolset.clone()]),
            ServerToolLoopConfig {
                max_consecutive_errors: 1,
                ..Default::default()
            },
            Arc::new(AllowAll),
        );
        let upstream = Arc::new(DisconnectUpstream::new(
            vec![
                exec(vec![
                    tool_call_with_id("search", "first-call"),
                    tool_call_with_id("search", "second-call"),
                ]),
                exec(vec![text("unexpected later turn")]),
            ],
            false,
        ));
        let task_upstream = Arc::clone(&upstream);

        let task = tokio::spawn(async move {
            loop_
                .run_with_provenance(&base_prompt(), &tool_ctx(), task_upstream.as_ref())
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), first_started.notified())
            .await
            .map_err(|_| {
                BitrouterError::Internal(
                    "timed out waiting for first disconnect test tool".to_string(),
                )
            })?;
        upstream.disconnect();
        first_release.add_permits(2);
        let outcome = task.await.map_err(|error| {
            BitrouterError::Internal(format!("disconnect batch test task failed: {error}"))
        })??;

        assert_eq!(upstream.calls(), 1);
        assert_eq!(toolset.calls(), 1);
        assert_eq!(toolset.finished(), 1);
        assert_eq!(outcome.result.server_tool_calls.len(), 1);
        assert_eq!(
            outcome.result.server_tool_calls[0].call_id.as_deref(),
            Some("first-call")
        );
        assert_eq!(
            outcome.result.server_tool_calls[0].status,
            ServerToolStatus::Error
        );
        assert!(!outcome.provider_terminal_exposed);
        assert_eq!(
            truncation_reason(&outcome.result),
            Some("client_disconnected")
        );
        Ok(())
    }

    #[tokio::test]
    async fn later_upstream_disconnect_preserves_prior_usage_and_server_call_evidence() -> Result<()>
    {
        let expected_usage = Usage {
            prompt_tokens: 17,
            completion_tokens: 6,
            reasoning_tokens: 2,
            cache_read_tokens: 4,
            origin: UsageOrigin::ProviderReported,
            raw: Some(Box::new(serde_json::json!({
                "input_tokens": 17,
                "output_tokens": 6
            }))),
            ..Default::default()
        };
        let provider_call = Content::ToolCall {
            id: "provider-call".to_string(),
            name: "web_search".to_string(),
            arguments: "{}".to_string(),
            provider_executed: true,
            dynamic: false,
            provider_metadata: ProviderMetadata::new(),
        };
        let mut first_result = exec(vec![
            tool_call_with_id("search", "router-call"),
            provider_call,
        ]);
        first_result.result.usage = Some(expected_usage.clone());
        let upstream = LaterDisconnectUpstream::new(vec![
            Ok(first_result),
            Err(BitrouterError::ClientDisconnected),
        ]);
        let toolset = Arc::new(CountingToolset::immediate());
        let loop_ = disconnect_loop_with(Arc::clone(&toolset));

        let outcome = loop_
            .run_with_provenance(&base_prompt(), &tool_ctx(), &upstream)
            .await?;

        assert_eq!(upstream.calls(), 2);
        assert_eq!(toolset.calls(), 1);
        assert_eq!(outcome.result.result.usage, Some(expected_usage));
        assert_eq!(outcome.result.server_tool_calls.len(), 2);
        assert!(outcome.result.server_tool_calls.iter().any(|call| {
            call.name == "search"
                && call.kind == ServerToolKind::Router
                && call.call_id.as_deref() == Some("router-call")
        }));
        assert!(outcome.result.server_tool_calls.iter().any(|call| {
            call.name == "web_search"
                && call.kind == ServerToolKind::Provider
                && call.call_id.as_deref() == Some("provider-call")
        }));
        assert!(!outcome.provider_terminal_exposed);
        assert_eq!(
            truncation_reason(&outcome.result),
            Some("client_disconnected")
        );
        Ok(())
    }

    #[tokio::test]
    async fn inject_replaces_caller_provider_defined_with_function() {
        // A caller declared `search` as a provider-defined (server) tool; the
        // registry owns `search`, so inject drops the raw declaration and
        // advertises the executable function form in its place.
        let loop_ = loop_with(&["search"], false, ServerToolLoopConfig::default());
        let mut base = base_prompt();
        base.tools.push(Tool::ProviderDefined {
            id: "demo.search".to_string(),
            name: "search".to_string(),
            args: serde_json::json!({ "engine": "x" }),
            provider_metadata: ProviderMetadata::new(),
        });
        let (working, owned) = loop_.inject(&base, &tool_ctx()).await.unwrap();
        assert!(owned.contains("search"));
        let search: Vec<&Tool> = working
            .tools
            .iter()
            .filter(|t| t.name() == "search")
            .collect();
        assert_eq!(search.len(), 1, "exactly one `search` tool remains");
        assert!(
            matches!(search[0], Tool::Function { .. }),
            "the remaining `search` is the executable function form"
        );
    }

    #[tokio::test]
    async fn inject_strips_namespaced_declaration_owned_under_a_bare_name() {
        // A toolset that owns by tail-match (like the Fusion toolset) advertises
        // the bare `fusion` but also owns the namespaced `bitrouter:fusion`. The
        // namespaced provider-defined declaration must be stripped so it never
        // reaches the upstream as an unknown server tool.
        struct TailToolset;
        #[async_trait]
        impl RouterToolset for TailToolset {
            async fn list_tools(&self, _c: &ToolContext) -> Result<Vec<Tool>> {
                Ok(vec![Tool::Function {
                    name: "fusion".to_string(),
                    description: None,
                    parameters: serde_json::json!({ "type": "object" }),
                    strict: None,
                    provider_metadata: ProviderMetadata::new(),
                }])
            }
            async fn call_tool(
                &self,
                _n: &str,
                _a: &str,
                _c: &ToolContext,
            ) -> Result<ToolResultOutput> {
                Ok(ToolResultOutput::Text {
                    value: "x".to_string(),
                })
            }
            fn owns(&self, name: &str) -> bool {
                name.rsplit([':', '.']).next().unwrap_or(name) == "fusion"
            }
        }

        let loop_ = ServerToolLoop::new(
            ToolsetRegistry::new(vec![Arc::new(TailToolset)]),
            ServerToolLoopConfig::default(),
            Arc::new(AllowAll),
        );
        let mut base = base_prompt();
        base.tools.push(Tool::ProviderDefined {
            id: "bitrouter.fusion".to_string(),
            name: "bitrouter:fusion".to_string(),
            args: serde_json::json!({}),
            provider_metadata: ProviderMetadata::new(),
        });
        let (working, _owned) = loop_.inject(&base, &tool_ctx()).await.unwrap();
        // Only the advertised bare `fusion` function remains.
        assert_eq!(working.tools.len(), 1);
        assert!(
            matches!(&working.tools[0], Tool::Function { name, .. } if name == "fusion"),
            "namespaced declaration stripped; executable function advertised"
        );
    }

    #[tokio::test]
    async fn inject_keeps_caller_provider_tool_the_registry_is_not_advertising() {
        // A toolset that *owns* `search` by tail-match but advertises nothing
        // this request (e.g. a declaration-gated tool like web_search with no
        // declaration). A caller's genuine provider-defined `search` must NOT be
        // stripped — otherwise the caller silently loses a real provider tool.
        struct SilentOwner;
        #[async_trait]
        impl RouterToolset for SilentOwner {
            async fn list_tools(&self, _c: &ToolContext) -> Result<Vec<Tool>> {
                Ok(Vec::new())
            }
            async fn call_tool(
                &self,
                _n: &str,
                _a: &str,
                _c: &ToolContext,
            ) -> Result<ToolResultOutput> {
                Ok(ToolResultOutput::Text {
                    value: "x".to_string(),
                })
            }
            fn owns(&self, name: &str) -> bool {
                name.rsplit([':', '.']).next().unwrap_or(name) == "search"
            }
        }

        let loop_ = ServerToolLoop::new(
            ToolsetRegistry::new(vec![Arc::new(SilentOwner)]),
            ServerToolLoopConfig::default(),
            Arc::new(AllowAll),
        );
        let mut base = base_prompt();
        base.tools.push(Tool::ProviderDefined {
            id: "demo.search".to_string(),
            name: "search".to_string(),
            args: serde_json::json!({}),
            provider_metadata: ProviderMetadata::new(),
        });
        let (working, owned) = loop_.inject(&base, &tool_ctx()).await.unwrap();
        assert!(owned.is_empty(), "nothing advertised this request");
        assert_eq!(
            working.tools.len(),
            1,
            "caller's provider tool is preserved"
        );
        assert!(
            matches!(&working.tools[0], Tool::ProviderDefined { name, .. } if name == "search")
        );
    }

    #[tokio::test]
    async fn executes_router_call_then_returns_text() {
        let loop_ = loop_with(&["search"], false, ServerToolLoopConfig::default());
        let upstream = ScriptedUpstream::new(vec![
            exec(vec![tool_call("search")]),
            exec(vec![text("the answer")]),
        ]);
        let result = loop_
            .run(&base_prompt(), &tool_ctx(), &upstream)
            .await
            .unwrap();
        let seen = upstream.seen();
        assert_eq!(seen.len(), 2);
        // The injected router tool is on the outbound prompt...
        assert!(seen[0].tools.iter().any(|t| t.name() == "search"));
        // ...and the tool-result turn is present on the second call.
        assert!(
            seen[1]
                .messages
                .iter()
                .any(|m| matches!(m.role, Role::Tool))
        );
        assert!(
            matches!(&result.result.content[0], Content::Text { text, .. } if text == "the answer")
        );
        // usage summed across the two iterations.
        assert_eq!(result.result.usage.unwrap().prompt_tokens, 2);
    }

    #[tokio::test]
    async fn hands_back_a_mixed_turn_without_executing() {
        let loop_ = loop_with(&["search"], false, ServerToolLoopConfig::default());
        let upstream = ScriptedUpstream::new(vec![exec(vec![
            tool_call("search"),
            tool_call("client_fn"),
        ])]);
        let result = loop_
            .run(&base_prompt(), &tool_ctx(), &upstream)
            .await
            .unwrap();
        assert_eq!(upstream.seen().len(), 1);
        assert!(matches!(
            &result.result.content[0],
            Content::ToolCall { .. }
        ));
    }

    #[tokio::test]
    async fn tool_error_is_fed_back_and_loop_continues() {
        let loop_ = loop_with(&["search"], true, ServerToolLoopConfig::default());
        let upstream = ScriptedUpstream::new(vec![
            exec(vec![tool_call("search")]),
            exec(vec![text("recovered")]),
        ]);
        let result = loop_
            .run(&base_prompt(), &tool_ctx(), &upstream)
            .await
            .unwrap();
        let seen = upstream.seen();
        assert_eq!(seen.len(), 2);
        // The fed-back tool result is an error block.
        let tool_msg = seen[1]
            .messages
            .iter()
            .find(|m| matches!(m.role, Role::Tool));
        assert!(matches!(
            tool_msg.and_then(|m| m.content.first()),
            Some(Content::ToolResult {
                output: ToolResultOutput::ErrorText { .. },
                ..
            })
        ));
        assert!(
            matches!(&result.result.content[0], Content::Text { text, .. } if text == "recovered")
        );
    }

    #[tokio::test]
    async fn terminates_at_max_iterations() {
        let config = ServerToolLoopConfig {
            max_iterations: 1,
            ..Default::default()
        };
        let loop_ = loop_with(&["search"], false, config);
        let upstream = ScriptedUpstream::new(vec![
            exec(vec![tool_call("search")]),
            exec(vec![tool_call("search")]),
        ]);
        let result = loop_
            .run(&base_prompt(), &tool_ctx(), &upstream)
            .await
            .unwrap();
        // round 0 executes (rounds 0 < 1), round 1 hits the cap.
        assert_eq!(upstream.seen().len(), 2);
        assert_eq!(
            result.result.finish_reason,
            Some(FinishReason::Other("max_tool_iterations".to_string()))
        );
    }

    #[tokio::test]
    async fn run_records_executed_router_call() {
        // Turn 1: model requests a router-owned tool ("subagent").
        // Turn 2: model returns final text — loop terminates Done.
        let loop_ = loop_with(&["subagent"], false, ServerToolLoopConfig::default());
        let upstream = ScriptedUpstream::new(vec![
            exec(vec![tool_call("subagent")]),
            exec(vec![text("done")]),
        ]);
        let result = loop_
            .run(&base_prompt(), &tool_ctx(), &upstream)
            .await
            .unwrap();
        let calls = &result.server_tool_calls;
        assert_eq!(calls.len(), 1, "exactly one server-tool call recorded");
        assert_eq!(calls[0].name, "subagent");
        assert_eq!(calls[0].kind, ServerToolKind::Router);
        assert_eq!(calls[0].status, ServerToolStatus::Ok);
    }

    #[tokio::test]
    async fn run_records_intermediate_provider_call() {
        // Turn 1: model issues BOTH a router-owned call ("subagent", provider_executed=false)
        // AND a provider-executed call ("web_search", provider_executed=true).
        // classify_turn skips provider_executed blocks when deciding disposition, so
        // the turn classifies as Execute.  Before this fix the provider-executed call
        // was silently dropped; after the fix it must appear in server_tool_calls.
        // Turn 2: model returns final text — loop terminates Done.
        let loop_ = loop_with(&["subagent"], false, ServerToolLoopConfig::default());

        let provider_call = Content::ToolCall {
            id: "web_search-1".to_string(),
            name: "web_search".to_string(),
            arguments: "{}".to_string(),
            provider_executed: true,
            dynamic: false,
            provider_metadata: ProviderMetadata::new(),
        };

        let upstream = ScriptedUpstream::new(vec![
            exec(vec![tool_call("subagent"), provider_call]),
            exec(vec![text("done")]),
        ]);
        let result = loop_
            .run(&base_prompt(), &tool_ctx(), &upstream)
            .await
            .unwrap();

        let calls = &result.server_tool_calls;

        // Expect exactly two recorded entries: one Router ("subagent") and one
        // Provider ("web_search").
        assert_eq!(
            calls.len(),
            2,
            "both the router call and the provider call must be recorded"
        );

        let router_entry = calls
            .iter()
            .find(|c| c.name == "subagent")
            .expect("Router entry for 'subagent' must be present");
        assert_eq!(router_entry.kind, ServerToolKind::Router);
        assert_eq!(router_entry.status, ServerToolStatus::Ok);

        let provider_entry = calls
            .iter()
            .find(|c| c.name == "web_search")
            .expect("Provider entry for 'web_search' must be present (intermediate-turn fix)");
        assert_eq!(provider_entry.kind, ServerToolKind::Provider);
        assert_eq!(provider_entry.status, ServerToolStatus::Ok);
    }
}
