//! The streaming stitcher: merge the N upstream streams of a tool loop into one
//! caller-visible [`StreamPart`] stream. Router tool calls are intercepted
//! (their raw `ToolCallDelta`s suppressed), executed, and re-emitted as
//! `ServerToolCall` + `ServerToolResult` parts; intermediate terminals are
//! suppressed; usage is accumulated into one final part; and a single terminal
//! `Finish` closes the merged stream. Keep-alive during tool execution is the
//! SSE layer's job (a gap in this part stream), not a `StreamPart`.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::StreamExt;

use super::classify::RouterCall;
use super::loop_controller::{ServerToolLoop, add_usage};
use super::toolset::ToolContext;
use crate::error::Result;
use crate::language_model::executor::StreamPartStream;
use crate::language_model::types::{
    Content, FinishReason, Message, Prompt, ProviderMetadata, Role, StreamPart, Usage,
};

/// One upstream streaming turn for a working prompt — the loop's callback into
/// the pipeline's streaming execution. A trait (not a closure) so the merged
/// stream has a concrete, `Send`, `'static` type.
#[async_trait]
pub trait UpstreamStream: Send + Sync {
    /// Open one upstream stream for `prompt`.
    async fn run(&self, prompt: &Prompt) -> Result<StreamPartStream>;
}

/// A tool call reconstructed from streamed `ToolCallDelta` fragments.
struct BufferedCall {
    id: String,
    name: String,
    args: String,
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
        // A forced first tool applies to the first turn only; restore the
        // caller's original choice after round 0 (mirrors the non-streaming loop).
        let base_tool_choice = base.tool_choice.clone();
        let ctx = ctx.clone();
        let loop_ = self;

        let stream = async_stream::stream! {
            let mut working = working;
            let mut total = Usage::default();
            let mut had_usage = false;
            let start = Instant::now();
            let mut rounds = 0u32;
            let mut consecutive_errors = 0u32;

            loop {
                let mut upstream_stream = match upstream.run(&working).await {
                    Ok(s) => s,
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                };

                let mut buffered: Vec<BufferedCall> = Vec::new();
                let mut assistant_text = String::new();
                let mut finish: Option<FinishReason> = None;

                // Forward narration in real time; buffer tool-call deltas;
                // accumulate usage; capture (but suppress) the terminal.
                while let Some(item) = upstream_stream.next().await {
                    let part = match item {
                        Ok(p) => p,
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    };
                    match part {
                        StreamPart::Usage { usage } => {
                            add_usage(&mut total, &usage);
                            had_usage = true;
                        }
                        StreamPart::Finish { reason } => finish = Some(reason),
                        StreamPart::ResponseCompleted { usage, .. } => {
                            // The Responses decoder carries usage only here (never
                            // a standalone `Usage` part), so it must be folded in
                            // or the loop under-bills a Responses upstream.
                            if let Some(u) = usage {
                                add_usage(&mut total, &u);
                                had_usage = true;
                            }
                            if finish.is_none() {
                                finish = Some(FinishReason::Stop);
                            }
                        }
                        StreamPart::ToolCallDelta {
                            id,
                            name,
                            arguments,
                        } => match buffered.iter_mut().find(|b| b.id == id) {
                            Some(b) => {
                                if let Some(n) = name.as_deref().filter(|n| !n.is_empty()) {
                                    b.name = n.to_string();
                                }
                                b.args.push_str(&arguments);
                            }
                            None => buffered.push(BufferedCall {
                                id,
                                name: name.unwrap_or_default(),
                                args: arguments,
                            }),
                        },
                        StreamPart::TextDelta { text } => {
                            assistant_text.push_str(&text);
                            yield Ok(StreamPart::TextDelta { text });
                        }
                        other => yield Ok(other),
                    }
                }

                // Drop malformed calls whose name never arrived — they are
                // neither router- nor client-executable, and would otherwise
                // force a spurious hand-back of an empty-named tool call.
                buffered.retain(|b| !b.name.is_empty());

                let has_client = buffered.iter().any(|b| !owned.contains(&b.name));
                let router: Vec<RouterCall> = buffered
                    .iter()
                    .filter(|b| owned.contains(&b.name))
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
                            });
                        }
                    }
                    if had_usage {
                        yield Ok(StreamPart::Usage { usage: total });
                    }
                    yield Ok(StreamPart::Finish {
                        reason: finish.unwrap_or(FinishReason::Stop),
                    });
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
                if rounds == 0 {
                    working.tool_choice = base_tool_choice.clone();
                }

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
    use crate::caller::CallerContext;
    use crate::language_model::server_tools::approval::AllowAll;
    use crate::language_model::server_tools::config::ServerToolLoopConfig;
    use crate::language_model::server_tools::toolset::{RouterToolset, ToolsetRegistry};
    use crate::language_model::types::{Tool, ToolResultOutput};
    use std::collections::VecDeque;
    use std::sync::Mutex;

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

    struct ScriptedStream {
        scripts: Mutex<VecDeque<Vec<StreamPart>>>,
        seen: Mutex<Vec<Prompt>>,
    }

    impl ScriptedStream {
        fn new(scripts: Vec<Vec<StreamPart>>) -> Self {
            Self {
                scripts: Mutex::new(scripts.into()),
                seen: Mutex::new(Vec::new()),
            }
        }
        fn seen(&self) -> Vec<Prompt> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl UpstreamStream for ScriptedStream {
        async fn run(&self, prompt: &Prompt) -> Result<StreamPartStream> {
            self.seen.lock().unwrap().push(prompt.clone());
            let parts = self.scripts.lock().unwrap().pop_front().unwrap();
            Ok(Box::pin(futures::stream::iter(parts.into_iter().map(Ok))))
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
        let upstream = Arc::new(ScriptedStream::new(scripts));
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
    async fn stream_forces_recall_then_restores_choice() {
        use crate::language_model::types::ToolChoice;
        let loop_ = Arc::new(
            ServerToolLoop::new(
                ToolsetRegistry::new(vec![Arc::new(OneTool)]),
                ServerToolLoopConfig::default(),
                Arc::new(AllowAll),
            )
            .with_forced_first_tool(Some("search".to_string())),
        );
        let scripts = vec![
            vec![
                StreamPart::ToolCallDelta {
                    id: "c1".into(),
                    name: Some("search".into()),
                    arguments: "{}".into(),
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
        let upstream = Arc::new(ScriptedStream::new(scripts));
        let _ = collect(
            loop_
                .run_stream(&base_prompt(), &tool_ctx(), upstream.clone())
                .await
                .unwrap(),
        )
        .await;
        let seen = upstream.seen();
        assert_eq!(seen.len(), 2);
        // Round 0 forces recall; round 1 reverts to the base choice (`None`).
        assert_eq!(
            seen[0].tool_choice,
            Some(ToolChoice::Tool {
                name: "search".to_string()
            })
        );
        assert_eq!(seen[1].tool_choice, None);
    }

    #[tokio::test]
    async fn hands_back_a_client_tool_call() {
        let scripts = vec![vec![
            StreamPart::ToolCallDelta {
                id: "x".into(),
                name: Some("client_fn".into()),
                arguments: "{}".into(),
            },
            StreamPart::Finish {
                reason: FinishReason::ToolCalls,
            },
        ]];
        let upstream = Arc::new(ScriptedStream::new(scripts));
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
                status: "completed".into(),
                usage: Some(Usage {
                    prompt_tokens: 7,
                    completion_tokens: 3,
                    ..Default::default()
                }),
            },
        ]];
        let upstream = Arc::new(ScriptedStream::new(scripts));
        let parts = collect(
            loop_()
                .run_stream(&base_prompt(), &tool_ctx(), upstream)
                .await
                .unwrap(),
        )
        .await;

        let usage = parts
            .iter()
            .find_map(|p| match p {
                StreamPart::Usage { usage } => Some(usage),
                _ => None,
            })
            .expect("a consolidated Usage part is emitted");
        assert_eq!(usage.prompt_tokens, 7);
        assert_eq!(usage.completion_tokens, 3);
    }
}
