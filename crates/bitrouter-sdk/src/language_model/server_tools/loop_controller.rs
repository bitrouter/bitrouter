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
    Content, ExecutionResult, FinishReason, Message, Prompt, ProviderMetadata, Role, Tool,
    ToolResultOutput, Usage,
};

/// One upstream turn for a working prompt — the loop's callback into the
/// pipeline's `execute_with_fallback`. A trait (rather than a closure) so the
/// resulting future has a concrete, `Send` type when the pipeline spawns the
/// request.
#[async_trait]
pub trait UpstreamTurn: Send + Sync {
    /// Run one upstream turn for `prompt`.
    async fn run(&self, prompt: &Prompt) -> Result<ExecutionResult>;
}

/// Drives the server-side tool loop over a [`ToolsetRegistry`].
pub struct ServerToolLoop {
    registry: ToolsetRegistry,
    config: ServerToolLoopConfig,
    approval: Arc<dyn ApprovalPolicy>,
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
    /// dropped from the working prompt for any name the registry owns: the
    /// toolset re-advertises that tool as an executable function tool, and the
    /// raw provider-defined form must not reach the upstream — it has no
    /// portable wire form and a same-protocol upstream would reject an unknown
    /// server tool. Dropping them also avoids a spurious self-collision below.
    pub(crate) async fn inject(
        &self,
        base: &Prompt,
        ctx: &ToolContext,
    ) -> Result<(Prompt, std::collections::BTreeSet<String>)> {
        let (injected, owned) = self.registry.list_all(ctx).await?;
        let mut working = base.clone();
        working
            .tools
            .retain(|t| !(matches!(t, Tool::ProviderDefined { .. }) && owned.contains(t.name())));
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
        let (mut working, owned) = self.inject(base, ctx).await?;

        let mut total = Usage::default();
        let mut had_usage = false;
        let start = Instant::now();
        let mut consecutive_errors = 0u32;
        let mut rounds = 0u32;

        loop {
            let mut result = upstream.run(&working).await?;
            if let Some(usage) = &result.result.usage {
                add_usage(&mut total, usage);
                had_usage = true;
            }

            match classify_turn(&result.result.content, &owned) {
                TurnDisposition::Done | TurnDisposition::HandBack => {
                    if had_usage {
                        result.result.usage = Some(total);
                    }
                    return Ok(result);
                }
                TurnDisposition::Execute(calls) => {
                    if rounds >= self.config.max_iterations
                        || start.elapsed() >= self.config.total_budget
                    {
                        return Ok(truncate(result, total, had_usage, "max_tool_iterations"));
                    }
                    let (tool_results, had_error) = self.execute_calls(&calls, ctx).await;
                    consecutive_errors = if had_error { consecutive_errors + 1 } else { 0 };
                    append_turn(&mut working, result.result.content.clone(), tool_results);
                    rounds += 1;
                    if consecutive_errors >= self.config.max_consecutive_errors {
                        return Ok(truncate(result, total, had_usage, "tool_errors"));
                    }
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
        if !self.approval.allow(call, ctx.caller()).await {
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

    /// Execute each router-owned call, returning the tool-result content blocks
    /// and whether any call produced an error.
    async fn execute_calls(&self, calls: &[RouterCall], ctx: &ToolContext) -> (Vec<Content>, bool) {
        let mut results = Vec::with_capacity(calls.len());
        let mut had_error = false;
        for call in calls {
            let (output, err) = self.call_one(call, ctx).await;
            had_error |= err;
            results.push(Content::ToolResult {
                call_id: call.id.clone(),
                tool_name: Some(call.name.clone()),
                output,
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            });
        }
        (results, had_error)
    }
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
    total.prompt_tokens += add.prompt_tokens;
    total.completion_tokens += add.completion_tokens;
    total.reasoning_tokens += add.reasoning_tokens;
    total.cache_read_tokens += add.cache_read_tokens;
    total.cache_write_tokens += add.cache_write_tokens;
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
    use crate::language_model::server_tools::approval::AllowAll;
    use crate::language_model::server_tools::toolset::RouterToolset;
    use crate::language_model::types::{GenerateResult, Tool};
    use std::collections::VecDeque;
    use std::sync::Mutex;

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
                }),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_details: None,
                provider_metadata: ProviderMetadata::new(),
            },
            latency_ms: 0,
            generation_time_ms: 0,
        }
    }

    fn tool_call(name: &str) -> Content {
        Content::ToolCall {
            id: format!("{name}-1"),
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
}
