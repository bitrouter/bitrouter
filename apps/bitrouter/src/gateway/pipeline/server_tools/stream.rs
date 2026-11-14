//! The streaming stitcher: merge the N upstream streams of a tool loop into one
//! caller-visible [`StreamPart`] stream. Router tool calls are intercepted
//! (their raw fragments and call boundaries suppressed), executed, and re-emitted as
//! `ServerToolCall` + `ServerToolResult` parts; intermediate terminals are
//! suppressed; received usage is published as cumulative snapshots; and a single terminal
//! `Finish` closes the merged stream. Keep-alive during tool execution is the
//! SSE layer's job (a gap in this part stream), not a `StreamPart`.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::StreamExt;

use super::classify::{RouterCall, permits_tool_execution};
use super::loop_controller::{ServerToolLoop, add_usage};
use super::toolset::ToolContext;
use crate::gateway::error::{BitrouterError, Result};
use crate::gateway::pipeline::executor::StreamPartStream;
use crate::gateway::pipeline::stream::UsageAccumulator;
use bitrouter_ai::types::{
    Content, FinishReason, Message, Prompt, ProviderMetadata, Role, StreamPart, ToolCallBoundary,
    Usage,
};

/// One upstream streaming turn for a working prompt — the loop's callback into
/// the pipeline's streaming execution. A trait (not a closure) so the merged
/// stream has a concrete, `Send`, `'static` type.
#[async_trait]
pub trait UpstreamStream: Send + Sync {
    /// Open one upstream stream for `prompt`.
    async fn run(&self, prompt: &Prompt) -> Result<StreamPartStream>;
}

/// Marks the pipeline's actual provider turn, whose HTTP EOF closes routing
/// custody independently of the enclosing logical server-tool request.
pub(crate) struct ProviderToolTurn;

/// A tool call reconstructed from streamed `ToolCallDelta` fragments.
struct BufferedCall {
    id: String,
    name: String,
    args: String,
    provider_metadata: ProviderMetadata,
    boundaries: Vec<ToolCallBoundary>,
}

fn invalid_tool_lifecycle(message: &str) -> BitrouterError {
    BitrouterError::UpstreamInvalidResponse {
        message: message.to_owned(),
        usage: None,
    }
}

fn cumulative_usage(
    completed: &Usage,
    had_completed: bool,
    current: &UsageAccumulator,
) -> Option<Usage> {
    let received = current.finalized();
    if !had_completed && received.is_none() {
        return None;
    }
    let mut total = completed.clone();
    if let Some(received) = received {
        add_usage(&mut total, &received);
    }
    Some(total)
}

/// Settlement gives embedded error usage precedence over accumulated stream
/// usage. Expose the same logical total on both paths at the loop boundary.
fn merge_error_usage(
    error: &mut BitrouterError,
    completed: &Usage,
    had_completed: bool,
    current: &mut UsageAccumulator,
) -> Option<Usage> {
    let BitrouterError::UpstreamInvalidResponse {
        usage: Some(embedded),
        ..
    } = error
    else {
        return None;
    };
    current.observe(&StreamPart::Usage {
        usage: embedded.as_ref().clone(),
    });
    let total = cumulative_usage(completed, had_completed, current)?;
    **embedded = total.clone();
    Some(total)
}

impl ServerToolLoop {
    /// Run the loop over streaming upstream turns, returning one merged stream.
    /// Mirrors [`ServerToolLoop::run`] but for the streaming path.
    pub async fn run_stream(
        self: Arc<Self>,
        base: &Prompt,
        ctx: &ToolContext,
        upstream: Arc<dyn UpstreamStream>,
    ) -> Result<StreamPartStream> {
        let (working, owned) = self.inject(base, ctx).await?;
        // `Result<StreamPartStream>` is the boundary between fallible request
        // setup and an HTTP response whose status may already be committed.
        // Open the first upstream turn eagerly so an HTTP 429/auth/availability
        // failure can still retain its real downstream status. Later turns are
        // driven inside the stream because earlier narration/tool activity may
        // already have reached the caller.
        let first_upstream_stream = upstream.run(&working).await?;
        let ctx = ctx.clone();
        let loop_ = self;

        let stream = async_stream::stream! {
            let mut working = working;
            let mut first_upstream_stream = Some(first_upstream_stream);
            let mut total = Usage::default();
            let mut had_usage = false;
            let start = Instant::now();
            let mut rounds = 0u32;
            let mut consecutive_errors = 0u32;

            loop {
                let mut upstream_stream = match first_upstream_stream.take() {
                    Some(stream) => stream,
                    None => match upstream.run(&working).await {
                        Ok(stream) => stream,
                        Err(mut error) => {
                            let mut opening_usage = UsageAccumulator::default();
                            if let Some(usage) = merge_error_usage(&mut error, &total, had_usage, &mut opening_usage) {
                                yield Ok(StreamPart::Usage { usage });
                            } else if had_usage {
                                yield Ok(StreamPart::Usage { usage: total });
                            }
                            yield Err(error);
                            return;
                        }
                    },
                };

                let mut buffered: Vec<BufferedCall> = Vec::new();
                let mut assistant_text = String::new();
                let mut finish: Option<FinishReason> = None;
                let mut response_completion = None;
                let mut turn_usage = UsageAccumulator::default();

                // Forward narration in real time; buffer tool-call deltas;
                // select one cumulative usage snapshot for this upstream turn;
                // capture (but suppress) the terminal. Publish completed turns
                // plus the current selected snapshot before polling again so
                // downstream custody retains received usage on error or drop.
                // Commit the current turn to the total only once at EOF.
                while let Some(item) = upstream_stream.next().await {
                    let part = match item {
                        Ok(p) => p,
                        Err(mut error) => {
                            if let Some(usage) = merge_error_usage(&mut error, &total, had_usage, &mut turn_usage) {
                                yield Ok(StreamPart::Usage { usage });
                            }
                            yield Err(error);
                            return;
                        }
                    };
                    match part {
                        StreamPart::Usage { usage } => {
                            turn_usage.observe(&StreamPart::Usage { usage });
                            if let Some(usage) = cumulative_usage(&total, had_usage, &turn_usage) {
                                yield Ok(StreamPart::Usage { usage });
                            }
                        }
                        StreamPart::Finish { reason } => finish = Some(reason),
                        StreamPart::ResponseCompleted {
                            id,
                            source_protocol,
                            status,
                            usage,
                            response_output_commitment,
                        } => {
                            // The Responses decoder carries usage only here (never
                            // a standalone `Usage` part), so it must be folded in
                            // or the loop under-bills a Responses upstream.
                            if let Some(u) = usage {
                                turn_usage.observe(&StreamPart::Usage { usage: u });
                                if let Some(usage) = cumulative_usage(&total, had_usage, &turn_usage) {
                                    yield Ok(StreamPart::Usage { usage });
                                }
                            }
                            if finish.is_none() {
                                finish = Some(FinishReason::Stop);
                            }
                            response_completion =
                                Some((id, source_protocol, status, response_output_commitment));
                        }
                        StreamPart::ToolCallDelta {
                            id,
                            name,
                            arguments,
                            provider_metadata,
                        } => match buffered.iter_mut().find(|b| b.id == id) {
                            Some(b) => {
                                if let Some(n) = name.as_deref().filter(|n| !n.is_empty()) {
                                    if !b.name.is_empty() {
                                        yield Err(invalid_tool_lifecycle("duplicate router tool call ID"));
                                        return;
                                    }
                                    b.name = n.to_owned();
                                }
                                if !b.boundaries.is_empty() {
                                    yield Err(invalid_tool_lifecycle("router tool arguments followed a close boundary"));
                                    return;
                                }
                                b.args.push_str(&arguments);
                                b.provider_metadata.extend(provider_metadata);
                            }
                            None => buffered.push(BufferedCall {
                                id,
                                name: name.unwrap_or_default(),
                                args: arguments,
                                provider_metadata,
                                boundaries: Vec::new(),
                            }),
                        },
                        StreamPart::ToolCallEnd { id, boundary } => {
                            let Some(call) = buffered.iter_mut().find(|call| call.id == id) else {
                                yield Err(invalid_tool_lifecycle("router tool boundary has no matching call"));
                                return;
                            };
                            call.boundaries.push(boundary);
                        }
                        StreamPart::TextDelta { text } => {
                            assistant_text.push_str(&text);
                            yield Ok(StreamPart::TextDelta { text });
                        }
                        other => yield Ok(other),
                    }
                }

                if let Some(usage) = turn_usage.finalized() {
                    add_usage(&mut total, &usage);
                    had_usage = true;
                }

                // Drop malformed calls whose name never arrived — they are
                // neither router- nor client-executable, and would otherwise
                // force a spurious hand-back of an empty-named tool call.
                buffered.retain(|b| !b.name.is_empty());

                let can_execute = permits_tool_execution(finish.as_ref())
                    && response_completion.as_ref().is_none_or(|(_, _, status, _)| status == "completed")
                    && buffered.iter().all(|call| call.boundaries.iter().all(|boundary| match boundary {
                        ToolCallBoundary::MessagesBlock { complete } | ToolCallBoundary::Turn { complete } => *complete,
                        ToolCallBoundary::ResponsesItem { status, .. } => {
                            status.as_deref().is_none_or(|status| status == "completed")
                        }
                        ToolCallBoundary::Arguments => true,
                    }));
                let has_client = buffered.iter().any(|b| !owned.contains(&b.name) || !b.provider_metadata.is_empty());
                let router: Vec<RouterCall> = buffered
                    .iter()
                    .filter(|b| can_execute && owned.contains(&b.name) && b.provider_metadata.is_empty())
                    .map(|b| RouterCall {
                        id: b.id.clone(),
                        name: b.name.clone(),
                        arguments: b.args.clone(),
                    })
                    .collect();

                // Terminal turns: a client-owned call present (hand the whole
                // turn back), or no router calls at all (final answer).
                if has_client || router.is_empty() {
                    if has_client {
                        for b in &buffered {
                            yield Ok(StreamPart::ToolCallDelta {
                                id: b.id.clone(),
                                name: Some(b.name.clone()),
                                arguments: b.args.clone(),
                                provider_metadata: b.provider_metadata.clone(),
                            });
                            for boundary in &b.boundaries {
                                yield Ok(StreamPart::ToolCallEnd { id: b.id.clone(), boundary: boundary.clone() });
                            }
                        }
                    }
                    if had_usage {
                        yield Ok(StreamPart::Usage { usage: total });
                    }
                    if let Some((id, source_protocol, status, response_output_commitment)) =
                        response_completion
                    {
                        // Preserve typed proof that this exposed terminal came
                        // from the final provider Responses completion. Usage
                        // remains on the consolidated `Usage` part above so
                        // cross-protocol framing stays unchanged.
                        yield Ok(StreamPart::ResponseCompleted {
                            id,
                            source_protocol,
                            status,
                            usage: None,
                            response_output_commitment,
                        });
                    } else {
                        yield Ok(StreamPart::Finish {
                            reason: finish.unwrap_or(FinishReason::Stop),
                        });
                    }
                    return;
                }

                // Bound the loop.
                if rounds >= loop_.config().max_iterations
                    || start.elapsed() >= loop_.config().total_budget
                {
                    if had_usage {
                        yield Ok(StreamPart::Usage { usage: total });
                    }
                    yield Ok(StreamPart::Finish {
                        reason: FinishReason::Other("max_tool_iterations".to_string()),
                    });
                    return;
                }

                // Execute each router call, surfacing the activity.
                let mut tool_results = Vec::with_capacity(router.len());
                let mut round_error = false;
                for call in &router {
                    let server_name = loop_.server_name_for(&call.name);
                    yield Ok(StreamPart::ServerToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        server_name,
                        dynamic: true,
                    });
                    let (output, err) = loop_.call_one(call, &ctx).await;
                    round_error |= err;
                    yield Ok(StreamPart::ServerToolResult {
                        call_id: call.id.clone(),
                        tool_name: Some(call.name.clone()),
                        output: output.clone(),
                        dynamic: true,
                    });
                    tool_results.push(Content::ToolResult {
                        call_id: call.id.clone(),
                        tool_name: Some(call.name.clone()),
                        output,
                        dynamic: false,
                        provider_metadata: ProviderMetadata::new(),
                    });
                }

                // Append the assistant tool-call turn + tool results so the next
                // upstream turn sees them.
                let mut assistant_content = Vec::new();
                if !assistant_text.is_empty() {
                    assistant_content.push(Content::Text {
                        text: std::mem::take(&mut assistant_text),
                        provider_metadata: ProviderMetadata::new(),
                    });
                }
                for call in &router {
                    assistant_content.push(Content::ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        provider_executed: false,
                        dynamic: false,
                        provider_metadata: ProviderMetadata::new(),
                    });
                }
                working.messages.push(Message {
                    role: Role::Assistant,
                    content: assistant_content,
                });
                working.messages.push(Message {
                    role: Role::Tool,
                    content: tool_results,
                });

                rounds += 1;
                consecutive_errors = if round_error { consecutive_errors + 1 } else { 0 };
                if consecutive_errors >= loop_.config().max_consecutive_errors {
                    if had_usage {
                        yield Ok(StreamPart::Usage { usage: total });
                    }
                    yield Ok(StreamPart::Finish {
                        reason: FinishReason::Other("tool_errors".to_string()),
                    });
                    return;
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::caller::CallerContext;
    use crate::gateway::pipeline::server_tools::approval::AllowAll;
    use crate::gateway::pipeline::server_tools::config::ServerToolLoopConfig;
    use crate::gateway::pipeline::server_tools::toolset::{RouterToolset, ToolsetRegistry};
    use bitrouter_ai::types::{ApiProtocol, Tool, ToolResultOutput};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct OneTool;

    #[async_trait]
    impl RouterToolset for OneTool {
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
            Ok(ToolResultOutput::Text {
                value: "ran".to_string(),
            })
        }
        fn owns(&self, name: &str) -> bool {
            name == "search"
        }
        fn server_name(&self) -> Option<&str> {
            Some("docs")
        }
    }

    struct CountingTool(Arc<AtomicUsize>);

    #[async_trait]
    impl RouterToolset for CountingTool {
        async fn list_tools(&self, ctx: &ToolContext) -> Result<Vec<Tool>> {
            OneTool.list_tools(ctx).await
        }

        async fn call_tool(
            &self,
            name: &str,
            arguments: &str,
            ctx: &ToolContext,
        ) -> Result<ToolResultOutput> {
            self.0.fetch_add(1, Ordering::SeqCst);
            OneTool.call_tool(name, arguments, ctx).await
        }

        fn owns(&self, name: &str) -> bool {
            OneTool.owns(name)
        }
    }

    struct RawMessages(Vec<crate::gateway::model::protocol::SseEvent>);

    #[async_trait]
    impl UpstreamStream for RawMessages {
        async fn run(&self, _: &Prompt) -> Result<StreamPartStream> {
            use crate::gateway::model::protocol::{OutboundAdapter, messages::MessagesAdapter};
            let events = self.0.clone();
            Ok(Box::pin(async_stream::try_stream! {
                let mut decoder = MessagesAdapter.stream_decoder();
                for event in events {
                    for part in decoder.decode(&event)? {
                        yield part;
                    }
                }
                for part in decoder.finish()? {
                    yield part;
                }
            }))
        }
    }

    #[tokio::test]
    async fn duplicate_messages_identity_cannot_execute_router_owned_tool() -> anyhow::Result<()> {
        use crate::gateway::model::protocol::SseEvent;
        use serde_json::json;

        let event = |kind: &str, mut value: serde_json::Value| {
            value["type"] = json!(kind);
            SseEvent {
                event: Some(kind.to_owned()),
                data: value.to_string(),
            }
        };
        let start = |index| {
            event(
                "content_block_start",
                json!({
                    "index":index,"content_block":{"type":"tool_use","id":"reused","name":"search","input":{}}
                }),
            )
        };
        let delta = |index, text| {
            event(
                "content_block_delta",
                json!({
                    "index":index,"delta":{"type":"input_json_delta","partial_json":text}
                }),
            )
        };
        let stop = |index| event("content_block_stop", json!({"index":index}));
        let call = |args: &str| StreamPart::ToolCallDelta {
            id: "reused".into(),
            name: Some("search".into()),
            arguments: args.into(),
            provider_metadata: Default::default(),
        };
        let raw = RawMessages(vec![
            event("message_start", json!({"message":{"id":"msg_duplicate"}})),
            start(0),
            delta(0, r#"{"safe":"#),
            stop(0),
            start(1),
            delta(1, "true}"),
            stop(1),
            event("message_delta", json!({"delta":{"stop_reason":"tool_use"}})),
            event("message_stop", json!({})),
        ]);
        let canonical = ScriptedStream {
            scripts: Mutex::new(VecDeque::from([vec![
                call(r#"{"safe":"#),
                call("true}"),
                StreamPart::Finish {
                    reason: FinishReason::ToolCalls,
                },
            ]])),
        };
        let upstreams: [Arc<dyn UpstreamStream>; 2] = [Arc::new(raw), Arc::new(canonical)];
        for upstream in upstreams {
            let calls = Arc::new(AtomicUsize::new(0));
            let loop_ = Arc::new(ServerToolLoop::new(
                ToolsetRegistry::new(vec![Arc::new(CountingTool(calls.clone()))]),
                ServerToolLoopConfig::default(),
                Arc::new(AllowAll),
            ));
            let mut stream = loop_
                .run_stream(&base_prompt(), &tool_ctx(), upstream)
                .await?;
            let mut rejected = false;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(part) => assert!(
                        !part.is_terminal() && !matches!(part, StreamPart::ServerToolCall { .. })
                    ),
                    Err(BitrouterError::UpstreamInvalidResponse { .. }) => rejected = true,
                    Err(error) => return Err(error.into()),
                }
            }
            assert!(rejected);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
        Ok(())
    }

    struct FailingUsageStream {
        turns: Mutex<VecDeque<Vec<Result<StreamPart>>>>,
    }

    #[async_trait]
    impl UpstreamStream for FailingUsageStream {
        async fn run(&self, _: &Prompt) -> Result<StreamPartStream> {
            let turn = self
                .turns
                .lock()
                .map_err(|_| BitrouterError::UpstreamUnavailable)?
                .pop_front()
                .ok_or(BitrouterError::UpstreamUnavailable)?;
            Ok(Box::pin(futures::stream::iter(turn)))
        }
    }

    #[tokio::test]
    async fn received_usage_survives_stitcher_error_and_cross_turn_accounting() -> anyhow::Result<()>
    {
        use bitrouter_ai::types::UsageOrigin;
        let usage = |input, output| StreamPart::Usage {
            usage: Usage {
                prompt_tokens: input,
                completion_tokens: output,
                origin: UsageOrigin::ProviderReported,
                ..Default::default()
            },
        };
        for prior_turn in [false, true] {
            let mut turns = VecDeque::new();
            if prior_turn {
                turns.push_back(vec![
                    Ok(usage(10, 2)),
                    Ok(usage(10, 3)),
                    Ok(StreamPart::ToolCallDelta {
                        id: "c1".into(),
                        name: Some("search".into()),
                        arguments: "{}".into(),
                        provider_metadata: Default::default(),
                    }),
                    Ok(StreamPart::Finish {
                        reason: FinishReason::ToolCalls,
                    }),
                ]);
            }
            turns.push_back(vec![
                Ok(usage(40, 7)),
                Ok(usage(40, 7)),
                Err(BitrouterError::UpstreamUnavailable),
            ]);
            let mut stream = loop_()
                .run_stream(
                    &base_prompt(),
                    &tool_ctx(),
                    Arc::new(FailingUsageStream {
                        turns: Mutex::new(turns),
                    }),
                )
                .await?;
            let mut received = Vec::new();
            let mut errors = 0;
            while let Some(part) = stream.next().await {
                match part {
                    Ok(StreamPart::Usage { usage }) => {
                        assert_eq!(usage.origin, UsageOrigin::ProviderReported);
                        received.push((usage.prompt_tokens, usage.completion_tokens));
                    }
                    Ok(part) => assert!(!part.is_terminal()),
                    Err(BitrouterError::UpstreamUnavailable) => {
                        assert_eq!(
                            received.last().copied(),
                            Some(if prior_turn { (50, 10) } else { (40, 7) })
                        );
                        errors += 1;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            assert_eq!(errors, 1);
        }
        Ok(())
    }

    struct EmbeddedUsageStream {
        turns: Mutex<VecDeque<Result<Vec<Result<StreamPart>>>>>,
    }

    #[async_trait]
    impl UpstreamStream for EmbeddedUsageStream {
        async fn run(&self, _: &Prompt) -> Result<StreamPartStream> {
            let parts = self
                .turns
                .lock()
                .map_err(|_| BitrouterError::UpstreamUnavailable)?
                .pop_front()
                .ok_or(BitrouterError::UpstreamUnavailable)??;
            Ok(Box::pin(futures::stream::iter(parts)))
        }
    }

    #[tokio::test]
    async fn embedded_usage_matches_cumulative_snapshot_on_drain_and_open_error()
    -> anyhow::Result<()> {
        use bitrouter_ai::types::UsageOrigin;
        use serde_json::json;
        let usage = |input, output| Usage {
            prompt_tokens: input,
            completion_tokens: output,
            origin: UsageOrigin::ProviderReported,
            raw: Some(Box::new(
                json!({"prompt_tokens":input,"completion_tokens":output}),
            )),
            ..Default::default()
        };
        for prior_turn in [false, true] {
            for opening in [false, true] {
                // A first-turn opening failure remains the eager setup error;
                // cross-turn opening failures occur inside the owned stream.
                if opening && !prior_turn {
                    continue;
                }
                for repeated_snapshot in [false, true] {
                    let mut turns = VecDeque::new();
                    if prior_turn {
                        turns.push_back(Ok(vec![
                            Ok(StreamPart::Usage {
                                usage: usage(10, 3),
                            }),
                            Ok(StreamPart::ToolCallDelta {
                                id: "c1".into(),
                                name: Some("search".into()),
                                arguments: "{}".into(),
                                provider_metadata: Default::default(),
                            }),
                            Ok(StreamPart::Finish {
                                reason: FinishReason::ToolCalls,
                            }),
                        ]));
                    }
                    let error = BitrouterError::UpstreamInvalidResponse {
                        message: "fixture embedded error".into(),
                        usage: Some(Box::new(usage(40, 7))),
                    };
                    if opening {
                        turns.push_back(Err(error));
                    } else {
                        let mut parts = Vec::new();
                        if repeated_snapshot {
                            parts.push(Ok(StreamPart::Usage {
                                usage: usage(40, 7),
                            }));
                            parts.push(Ok(StreamPart::Usage {
                                usage: usage(40, 7),
                            }));
                        }
                        parts.push(Err(error));
                        turns.push_back(Ok(parts));
                    }
                    let calls = Arc::new(AtomicUsize::new(0));
                    let loop_ = Arc::new(ServerToolLoop::new(
                        ToolsetRegistry::new(vec![Arc::new(CountingTool(calls.clone()))]),
                        ServerToolLoopConfig::default(),
                        Arc::new(AllowAll),
                    ));
                    let mut stream = loop_
                        .run_stream(
                            &base_prompt(),
                            &tool_ctx(),
                            Arc::new(EmbeddedUsageStream {
                                turns: Mutex::new(turns),
                            }),
                        )
                        .await?;
                    let mut latest = None;
                    let mut errors = 0;
                    while let Some(part) = stream.next().await {
                        match part {
                            Ok(StreamPart::Usage { usage }) => latest = Some(usage),
                            Ok(part) => assert!(!part.is_terminal()),
                            Err(BitrouterError::UpstreamInvalidResponse {
                                usage: Some(embedded),
                                ..
                            }) => {
                                let expected = if prior_turn { (50, 10) } else { (40, 7) };
                                assert_eq!(
                                    (embedded.prompt_tokens, embedded.completion_tokens),
                                    expected
                                );
                                let latest = latest.as_ref().ok_or_else(|| {
                                    anyhow::anyhow!("missing cumulative snapshot")
                                })?;
                                assert_eq!(
                                    (latest.prompt_tokens, latest.completion_tokens),
                                    expected
                                );
                                assert_eq!(embedded.origin, UsageOrigin::ProviderReported);
                                assert_eq!(embedded.raw, latest.raw);
                                let raw = embedded
                                    .raw
                                    .as_deref()
                                    .ok_or_else(|| anyhow::anyhow!("missing raw evidence"))?;
                                assert_eq!(
                                    raw,
                                    &if prior_turn {
                                        json!({"provider_usage_fragments":[{"prompt_tokens":10,"completion_tokens":3},{"prompt_tokens":40,"completion_tokens":7}]})
                                    } else {
                                        json!({"prompt_tokens":40,"completion_tokens":7})
                                    }
                                );
                                errors += 1;
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                    assert_eq!(errors, 1);
                    assert_eq!(calls.load(Ordering::SeqCst), usize::from(prior_turn));
                }
            }
        }
        Ok(())
    }

    struct ScriptedStream {
        scripts: Mutex<VecDeque<Vec<StreamPart>>>,
    }

    #[async_trait]
    impl UpstreamStream for ScriptedStream {
        async fn run(&self, _prompt: &Prompt) -> Result<StreamPartStream> {
            let parts = self.scripts.lock().unwrap().pop_front().unwrap();
            Ok(Box::pin(futures::stream::iter(parts.into_iter().map(Ok))))
        }
    }

    struct FailingStartStream;

    #[async_trait]
    impl UpstreamStream for FailingStartStream {
        async fn run(&self, _prompt: &Prompt) -> Result<StreamPartStream> {
            Err(crate::gateway::error::BitrouterError::UpstreamRateLimited {
                retry_after: Some(9),
                detail: None,
            })
        }
    }

    struct FailingSecondTurnStream {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl UpstreamStream for FailingSecondTurnStream {
        async fn run(&self, _prompt: &Prompt) -> Result<StreamPartStream> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let parts = vec![
                    StreamPart::ToolCallDelta {
                        id: "c1".into(),
                        name: Some("search".into()),
                        arguments: "{}".into(),
                        provider_metadata: Default::default(),
                    },
                    StreamPart::Finish {
                        reason: FinishReason::ToolCalls,
                    },
                ];
                return Ok(Box::pin(futures::stream::iter(parts.into_iter().map(Ok))));
            }
            Err(crate::gateway::error::BitrouterError::UpstreamTimeout)
        }
    }

    fn loop_() -> Arc<ServerToolLoop> {
        Arc::new(ServerToolLoop::new(
            ToolsetRegistry::new(vec![Arc::new(OneTool)]),
            ServerToolLoopConfig::default(),
            Arc::new(AllowAll),
        ))
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
            stream: true,
        }
    }

    async fn collect(stream: StreamPartStream) -> Vec<StreamPart> {
        let mut stream = stream;
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            out.push(item.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn no_tool_choice_hands_back_streamed_calls_without_a_second_turn() -> Result<()> {
        let mut prompt = base_prompt();
        prompt.tool_choice = Some(bitrouter_ai::types::ToolChoice::None);
        let upstream = Arc::new(FailingSecondTurnStream {
            calls: AtomicUsize::new(0),
        });
        let mut stream = loop_()
            .run_stream(&prompt, &tool_ctx(), upstream.clone())
            .await?;
        let mut parts = Vec::new();
        while let Some(part) = stream.next().await {
            parts.push(part?);
        }
        assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);
        assert!(
            parts
                .iter()
                .any(|part| matches!(part, StreamPart::ToolCallDelta { .. }))
        );
        assert!(
            !parts
                .iter()
                .any(|part| matches!(part, StreamPart::ServerToolCall { .. }))
        );
        Ok(())
    }

    #[tokio::test]
    async fn first_upstream_failure_is_returned_before_stream_start() {
        let result = loop_()
            .run_stream(&base_prompt(), &tool_ctx(), Arc::new(FailingStartStream))
            .await;

        assert!(matches!(
            result,
            Err(crate::gateway::error::BitrouterError::UpstreamRateLimited {
                retry_after: Some(9),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn later_turn_failure_remains_an_in_stream_error() {
        let result = loop_()
            .run_stream(
                &base_prompt(),
                &tool_ctx(),
                Arc::new(FailingSecondTurnStream {
                    calls: AtomicUsize::new(0),
                }),
            )
            .await;
        assert!(
            result.is_ok(),
            "first turn unexpectedly failed: {:?}",
            result.as_ref().err()
        );
        let Ok(mut stream) = result else {
            return;
        };
        let mut terminal_error = None;
        while let Some(item) = stream.next().await {
            if let Err(error) = item {
                terminal_error = Some(error);
                break;
            }
        }

        assert!(matches!(
            terminal_error,
            Some(crate::gateway::error::BitrouterError::UpstreamTimeout)
        ));
    }

    #[tokio::test]
    async fn stitches_router_tool_call_into_one_continuous_stream() {
        let scripts = vec![
            vec![
                StreamPart::TextDelta {
                    text: "searching ".into(),
                },
                StreamPart::ToolCallDelta {
                    id: "c1".into(),
                    name: Some("search".into()),
                    arguments: "{}".into(),
                    provider_metadata: Default::default(),
                },
                StreamPart::Finish {
                    reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                StreamPart::TextDelta {
                    text: "answer".into(),
                },
                StreamPart::Finish {
                    reason: FinishReason::Stop,
                },
            ],
        ];
        let upstream = Arc::new(ScriptedStream {
            scripts: Mutex::new(scripts.into()),
        });
        let parts = collect(
            loop_()
                .run_stream(&base_prompt(), &tool_ctx(), upstream)
                .await
                .unwrap(),
        )
        .await;

        // Exactly one terminal Finish.
        assert_eq!(
            parts
                .iter()
                .filter(|p| matches!(p, StreamPart::Finish { .. }))
                .count(),
            1
        );
        // The router call is surfaced as ServerToolCall + ServerToolResult...
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, StreamPart::ServerToolCall { name, server_name, .. } if name == "search" && server_name.as_deref() == Some("docs")))
        );
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, StreamPart::ServerToolResult { .. }))
        );
        // ...and the raw router ToolCallDelta is NOT forwarded.
        assert!(
            !parts
                .iter()
                .any(|p| matches!(p, StreamPart::ToolCallDelta { .. }))
        );
        // Both turns' text streamed through, in order.
        let text: String = parts
            .iter()
            .filter_map(|p| match p {
                StreamPart::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "searching answer");
    }

    #[tokio::test]
    async fn hands_back_a_client_tool_call() {
        let scripts = vec![vec![
            StreamPart::ToolCallDelta {
                id: "x".into(),
                name: Some("client_fn".into()),
                arguments: "{}".into(),
                provider_metadata: Default::default(),
            },
            StreamPart::Finish {
                reason: FinishReason::ToolCalls,
            },
        ]];
        let upstream = Arc::new(ScriptedStream {
            scripts: Mutex::new(scripts.into()),
        });
        let parts = collect(
            loop_()
                .run_stream(&base_prompt(), &tool_ctx(), upstream)
                .await
                .unwrap(),
        )
        .await;

        // The client tool call is forwarded for the caller to execute...
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, StreamPart::ToolCallDelta { name, .. } if name.as_deref() == Some("client_fn")))
        );
        // ...nothing was executed server-side.
        assert!(
            !parts
                .iter()
                .any(|p| matches!(p, StreamPart::ServerToolCall { .. }))
        );
        assert_eq!(
            parts
                .iter()
                .filter(|p| matches!(p, StreamPart::Finish { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn accumulates_usage_from_response_completed() {
        // A Responses-style upstream carries usage only on ResponseCompleted
        // (never a standalone Usage part); the stitcher must still emit a
        // consolidated Usage so settlement bills the authoritative count.
        let scripts = vec![vec![
            StreamPart::TextDelta { text: "hi".into() },
            StreamPart::ResponseCompleted {
                id: "r1".into(),
                source_protocol: ApiProtocol::Responses,
                status: "completed".into(),
                usage: Some(Usage {
                    prompt_tokens: 7,
                    completion_tokens: 3,
                    ..Default::default()
                }),
                response_output_commitment: None,
            },
        ]];
        let upstream = Arc::new(ScriptedStream {
            scripts: Mutex::new(scripts.into()),
        });
        let parts = collect(
            loop_()
                .run_stream(&base_prompt(), &tool_ctx(), upstream)
                .await
                .unwrap(),
        )
        .await;

        let usage = parts
            .iter()
            .rev()
            .find_map(|p| match p {
                StreamPart::Usage { usage } => Some(usage),
                _ => None,
            })
            .expect("a consolidated Usage part is emitted");
        assert_eq!(usage.prompt_tokens, 7);
        assert_eq!(usage.completion_tokens, 3);
    }

    #[tokio::test]
    async fn deduplicates_cumulative_usage_within_one_upstream_turn() {
        // Some OpenAI-compatible providers emit a cumulative usage snapshot
        // with the finish chunk and then an enriched trailing snapshot. The
        // server-tool stitcher must select one snapshot for this turn, while
        // retaining details that exist only on the authoritative final frame.
        let scripts = vec![vec![
            StreamPart::Usage {
                usage: Usage {
                    prompt_tokens: 5_140,
                    completion_tokens: 61,
                    ..Default::default()
                },
            },
            StreamPart::Usage {
                usage: Usage {
                    prompt_tokens: 5_140,
                    completion_tokens: 61,
                    cache_read_tokens: 4_608,
                    ..Default::default()
                },
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ]];
        let upstream = Arc::new(ScriptedStream {
            scripts: Mutex::new(scripts.into()),
        });
        let parts = collect(
            loop_()
                .run_stream(&base_prompt(), &tool_ctx(), upstream)
                .await
                .unwrap(),
        )
        .await;

        let usage = parts
            .iter()
            .rev()
            .find_map(|part| match part {
                StreamPart::Usage { usage } => Some(usage),
                _ => None,
            })
            .expect("one consolidated usage part is emitted");
        assert_eq!(usage.prompt_tokens, 5_140);
        assert_eq!(usage.completion_tokens, 61);
        assert_eq!(usage.cache_read_tokens, 4_608);
    }

    #[tokio::test]
    async fn sums_selected_usage_across_real_tool_rounds() {
        let scripts = vec![
            vec![
                StreamPart::Usage {
                    usage: Usage {
                        prompt_tokens: 10,
                        completion_tokens: 2,
                        ..Default::default()
                    },
                },
                StreamPart::Usage {
                    usage: Usage {
                        prompt_tokens: 10,
                        completion_tokens: 3,
                        ..Default::default()
                    },
                },
                StreamPart::ToolCallDelta {
                    id: "c1".into(),
                    name: Some("search".into()),
                    arguments: "{}".into(),
                    provider_metadata: Default::default(),
                },
                StreamPart::Finish {
                    reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                StreamPart::Usage {
                    usage: Usage {
                        prompt_tokens: 20,
                        completion_tokens: 4,
                        ..Default::default()
                    },
                },
                StreamPart::Usage {
                    usage: Usage {
                        prompt_tokens: 20,
                        completion_tokens: 5,
                        cache_read_tokens: 8,
                        ..Default::default()
                    },
                },
                StreamPart::Finish {
                    reason: FinishReason::Stop,
                },
            ],
        ];
        let upstream = Arc::new(ScriptedStream {
            scripts: Mutex::new(scripts.into()),
        });
        let parts = collect(
            loop_()
                .run_stream(&base_prompt(), &tool_ctx(), upstream)
                .await
                .unwrap(),
        )
        .await;

        let usage = parts
            .iter()
            .rev()
            .find_map(|part| match part {
                StreamPart::Usage { usage } => Some(usage),
                _ => None,
            })
            .expect("one cross-round usage total is emitted");
        assert_eq!(usage.prompt_tokens, 30);
        assert_eq!(usage.completion_tokens, 8);
        assert_eq!(usage.cache_read_tokens, 8);
    }
}
