//! # bitrouter-bedrock
//!
//! Amazon Bedrock outbound provider for bitrouter.
//!
//! This crate plugs an [`Executor`] for [Amazon Bedrock](https://aws.amazon.com/bedrock/)
//! into the bitrouter pipeline. It uses the [official AWS Rust SDK][sdk] —
//! the SDK owns endpoint resolution, [AWS SigV4][sigv4] signing of every
//! request, retries, credential resolution, and the binary event-stream
//! framing used for streaming responses.
//!
//! [sdk]: https://docs.rs/aws-sdk-bedrockruntime/
//! [sigv4]: https://docs.aws.amazon.com/general/latest/gr/sigv4_signing.html
//!
//! Inference goes through Bedrock's [Converse] / [ConverseStream] API rather
//! than the older `InvokeModel` — Converse is the model-family-agnostic shape
//! (one wire shape across Anthropic Claude, Meta Llama, Mistral, Amazon Titan,
//! Amazon Nova, etc.), and it maps almost 1:1 to bitrouter's canonical
//! [`Prompt`] / [`GenerateResult`].
//!
//! [Converse]: https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html
//! [ConverseStream]: https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html
//!
//! ## Wiring
//!
//! Bedrock does not fit the SDK's `OutboundAdapter` + `Transport` pattern —
//! the AWS SDK builds and signs its own HTTP requests so there is no
//! `reqwest::Request` for a `Transport::authorise` impl to sign. Instead,
//! [`BedrockExecutor`] implements [`Executor`] directly. Combine it with
//! the default [`HttpExecutor`](bitrouter_sdk::language_model::HttpExecutor)
//! under a [`DispatchExecutor`](bitrouter_sdk::language_model::DispatchExecutor)
//! so the four built-in HTTP protocols continue to use `HttpExecutor` and
//! Bedrock targets land on `BedrockExecutor`:
//!
//! ```no_run
//! use std::sync::Arc;
//! use bitrouter_sdk::App;
//! use bitrouter_sdk::language_model::{
//!     ApiProtocol, DispatchExecutor, Executor, HttpExecutor, StaticRoutingTable,
//! };
//! use bitrouter_bedrock::BedrockExecutor;
//!
//! # async fn run() -> bitrouter_sdk::Result<()> {
//! let http: Arc<dyn Executor> = Arc::new(HttpExecutor::with_defaults()?);
//! let bedrock: Arc<dyn Executor> = Arc::new(BedrockExecutor::from_env().await);
//! let executor = DispatchExecutor::new(http)
//!     .with(ApiProtocol::Custom("bedrock-claude".into()), bedrock);
//!
//! let _app = App::builder()
//!     .language_model(|lm| {
//!         lm.routing_table(Arc::new(StaticRoutingTable::new()))
//!           .executor(Arc::new(executor));
//!     })
//!     .build()?;
//! # Ok(()) }
//! ```
//!
//! Targets that should land on Bedrock carry `api_protocol: bedrock-claude`
//! in `bitrouter.yaml` (mapped to [`ApiProtocol::Custom`](bitrouter_sdk::language_model::ApiProtocol::Custom))
//! and use the Bedrock model id as `service_id` (e.g.
//! `anthropic.claude-3-5-sonnet-20241022-v2:0`, or a cross-region inference
//! profile id like `us.anthropic.claude-3-5-sonnet-20240620-v1:0`).
//!
//! ## Credentials
//!
//! [`BedrockExecutor::from_env`] uses [`aws_config::load_from_env`], which
//! resolves credentials from the [standard AWS credential chain][chain]:
//! environment variables, `~/.aws/credentials`, IAM Roles for Service Accounts
//! (IRSA), EC2 instance metadata, etc. The region must also be resolved
//! (typically from `AWS_REGION`).
//!
//! [chain]: https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::time::Instant;

use async_trait::async_trait;

use aws_sdk_bedrockruntime as bedrock;
use aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamOutput as ConverseStreamCall;
use aws_sdk_bedrockruntime::types::{
    ContentBlock, ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStart,
    ContentBlockStartEvent, ConversationRole, ConverseOutput, ConverseStreamMetadataEvent,
    ConverseStreamOutput, InferenceConfiguration, Message as BedrockMessage, MessageStopEvent,
    StopReason, SystemContentBlock, ToolResultBlock, ToolResultContentBlock, ToolUseBlock,
    ToolUseBlockStart,
};
use aws_smithy_types::Document;
use aws_smithy_types::event_stream::RawMessage;
use bedrock::error::SdkError;

use bitrouter_sdk::language_model::{
    Content, ExecutionResult, Executor, FinishReason, GenerateResult, Prompt, Role, RoutingTarget,
    StreamPart, StreamPartStream, Usage,
};
use bitrouter_sdk::{BitrouterError, Result};

/// Outbound [`Executor`] for Amazon Bedrock, using the Converse /
/// ConverseStream API of the [`aws_sdk_bedrockruntime`] crate.
///
/// Build with [`from_env`](Self::from_env) (which uses the standard AWS
/// credential and region chain) or [`from_client`](Self::from_client) (when
/// you want to supply a pre-built SDK client, e.g. in tests or to use a
/// non-default credential provider).
pub struct BedrockExecutor {
    client: bedrock::Client,
}

impl BedrockExecutor {
    /// Build an executor whose underlying AWS client loads its region +
    /// credentials from the standard AWS chain (env vars, `~/.aws/...`,
    /// IRSA / IMDS, …). Equivalent to:
    ///
    /// ```ignore
    /// let cfg = aws_config::load_from_env().await;
    /// BedrockExecutor::from_client(aws_sdk_bedrockruntime::Client::new(&cfg))
    /// ```
    pub async fn from_env() -> Self {
        // aws_config::load_from_env resolves region + credentials per
        // https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html.
        let config = aws_config::load_from_env().await;
        Self::from_client(bedrock::Client::new(&config))
    }

    /// Build an executor over a pre-constructed Bedrock Runtime client.
    pub fn from_client(client: bedrock::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Executor for BedrockExecutor {
    async fn execute(&self, target: &RoutingTarget, prompt: &Prompt) -> Result<ExecutionResult> {
        // https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html
        let mut builder = self.client.converse().model_id(&target.service_id);

        if let Some(sys) = &prompt.system
            && !sys.is_empty()
        {
            builder = builder.system(SystemContentBlock::Text(sys.clone()));
        }

        for message in &prompt.messages {
            let role = canonical_role_to_bedrock(message.role)?;
            let blocks = canonical_content_to_bedrock(&message.content)?;
            // A bitrouter message with no Bedrock-mappable content (e.g. only
            // Reasoning blocks coming back the other way) becomes an empty
            // message; Bedrock rejects empty `content`, so skip those.
            if blocks.is_empty() {
                continue;
            }
            let bedrock_msg = BedrockMessage::builder()
                .role(role)
                .set_content(Some(blocks))
                .build()
                .map_err(|e| BitrouterError::internal(format!("building Bedrock message: {e}")))?;
            builder = builder.messages(bedrock_msg);
        }

        builder = builder.inference_config(build_inference_config(prompt));

        let started = Instant::now();
        let response = builder.send().await.map_err(map_bedrock_err)?;
        let elapsed = started.elapsed().as_millis() as u64;

        let stop_reason = response.stop_reason.clone();
        let usage = response.usage.as_ref().map(token_usage_to_canonical);

        let output_msg = match response.output {
            Some(ConverseOutput::Message(m)) => m,
            _ => {
                return Err(BitrouterError::Upstream {
                    status: 502,
                    message: "Bedrock Converse response has no output.message".to_string(),
                });
            }
        };

        let content = bedrock_content_to_canonical(output_msg.content());

        Ok(ExecutionResult {
            provider_id: target.provider_name.clone(),
            model_id: target.service_id.clone(),
            account_label: target.account_label.clone(),
            result: GenerateResult {
                content,
                usage,
                finish_reason: bedrock_stop_to_finish(&stop_reason),
            },
            latency_ms: elapsed,
            generation_time_ms: elapsed,
        })
    }

    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
    ) -> Result<StreamPartStream> {
        // https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html
        let mut builder = self.client.converse_stream().model_id(&target.service_id);

        if let Some(sys) = &prompt.system
            && !sys.is_empty()
        {
            builder = builder.system(SystemContentBlock::Text(sys.clone()));
        }
        for message in &prompt.messages {
            let role = canonical_role_to_bedrock(message.role)?;
            let blocks = canonical_content_to_bedrock(&message.content)?;
            if blocks.is_empty() {
                continue;
            }
            let bedrock_msg = BedrockMessage::builder()
                .role(role)
                .set_content(Some(blocks))
                .build()
                .map_err(|e| BitrouterError::internal(format!("building Bedrock message: {e}")))?;
            builder = builder.messages(bedrock_msg);
        }
        builder = builder.inference_config(build_inference_config(prompt));

        let response: ConverseStreamCall = builder.send().await.map_err(map_bedrock_err)?;
        // The SDK exposes the event stream as an `EventReceiver`. Convert each
        // upstream event into one or more canonical [`StreamPart`]s.
        // Ref: https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStreamOutput.html
        let mut stream = response.stream;

        let out = async_stream::stream! {
            // Tool-call id resolution: Bedrock emits ContentBlockStart with
            // the tool id/name, then ContentBlockDelta chunks with the
            // function-arguments JSON, then ContentBlockStop. The bitrouter
            // canonical `ToolCallDelta` ships the id on each delta, so cache
            // the open block's id and emit it on every fragment.
            let mut tool_id_by_index: std::collections::HashMap<i32, String> =
                std::collections::HashMap::new();

            loop {
                match stream.recv().await {
                    Ok(Some(event)) => {
                        for part in event_to_parts(event, &mut tool_id_by_index) {
                            yield Ok(part);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        yield Err(map_stream_err(e));
                        return;
                    }
                }
            }
        };

        Ok(Box::pin(out))
    }
}

// ===== canonical → Bedrock =====

fn canonical_role_to_bedrock(role: Role) -> Result<ConversationRole> {
    // Bedrock Converse roles: user | assistant.
    // https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Message.html
    // System messages go in the top-level `system` field; tool results ride
    // inside a user-turn `toolResult` block — there is no Tool role on the
    // wire, so bitrouter `Role::Tool` maps to user here and the content
    // becomes a `ToolResult` block.
    match role {
        Role::User | Role::Tool => Ok(ConversationRole::User),
        Role::Assistant => Ok(ConversationRole::Assistant),
        Role::System => Err(BitrouterError::bad_request(
            "Bedrock Converse: system messages belong in `system`, not `messages`",
        )),
    }
}

fn canonical_content_to_bedrock(blocks: &[Content]) -> Result<Vec<ContentBlock>> {
    // https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ContentBlock.html
    let mut out = Vec::with_capacity(blocks.len());
    for c in blocks {
        match c {
            Content::Text { text } => out.push(ContentBlock::Text(text.clone())),
            Content::ToolCall {
                id,
                name,
                arguments,
            } => {
                let args_doc = json_to_document(arguments)?;
                let tool_use = ToolUseBlock::builder()
                    .tool_use_id(id.clone())
                    .name(name.clone())
                    .input(args_doc)
                    .build()
                    .map_err(|e| {
                        BitrouterError::internal(format!("building Bedrock ToolUseBlock: {e}"))
                    })?;
                out.push(ContentBlock::ToolUse(tool_use));
            }
            Content::ToolResult { call_id, content } => {
                let tool_result = ToolResultBlock::builder()
                    .tool_use_id(call_id.clone())
                    .content(ToolResultContentBlock::Text(content.clone()))
                    .build()
                    .map_err(|e| {
                        BitrouterError::internal(format!("building Bedrock ToolResultBlock: {e}"))
                    })?;
                out.push(ContentBlock::ToolResult(tool_result));
            }
            Content::Reasoning { .. } => {
                // Reasoning blocks on the request side are not part of the
                // Bedrock Converse input schema — they appear only on the
                // *response* side (under `reasoningContent`). Skipping.
            }
        }
    }
    Ok(out)
}

fn build_inference_config(prompt: &Prompt) -> InferenceConfiguration {
    // https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_InferenceConfiguration.html
    let mut b = InferenceConfiguration::builder();
    if let Some(t) = prompt.params.temperature {
        b = b.temperature(t as f32);
    }
    if let Some(p) = prompt.params.top_p {
        b = b.top_p(p as f32);
    }
    if let Some(m) = prompt.params.max_tokens {
        b = b.max_tokens(m as i32);
    }
    b.build()
}

/// Best-effort JSON ↔ `aws_smithy_types::Document` conversion. Document is
/// AWS's union of `null|bool|number|string|array|object`; the cases we hit
/// at the API surface are scalars + nested objects (function arguments).
fn json_to_document(s: &str) -> Result<Document> {
    let v: serde_json::Value = serde_json::from_str(s)
        .map_err(|e| BitrouterError::internal(format!("invalid JSON tool arguments: {e}")))?;
    Ok(json_value_to_document(v))
}

fn json_value_to_document(v: serde_json::Value) -> Document {
    use serde_json::Value::*;
    match v {
        Null => Document::Null,
        Bool(b) => Document::Bool(b),
        Number(n) => {
            if let Some(i) = n.as_i64() {
                Document::Number(aws_smithy_types::Number::NegInt(i))
            } else if let Some(u) = n.as_u64() {
                Document::Number(aws_smithy_types::Number::PosInt(u))
            } else {
                Document::Number(aws_smithy_types::Number::Float(n.as_f64().unwrap_or(0.0)))
            }
        }
        String(s) => Document::String(s),
        Array(items) => Document::Array(items.into_iter().map(json_value_to_document).collect()),
        Object(map) => {
            let entries = map
                .into_iter()
                .map(|(k, v)| (k, json_value_to_document(v)))
                .collect();
            Document::Object(entries)
        }
    }
}

// ===== Bedrock → canonical =====

fn bedrock_content_to_canonical(blocks: &[ContentBlock]) -> Vec<Content> {
    let mut out = Vec::with_capacity(blocks.len());
    for b in blocks {
        match b {
            ContentBlock::Text(t) => out.push(Content::Text { text: t.clone() }),
            ContentBlock::ToolUse(tu) => {
                let arguments = document_to_json(tu.input()).to_string();
                out.push(Content::ToolCall {
                    id: tu.tool_use_id().to_string(),
                    name: tu.name().to_string(),
                    arguments,
                });
            }
            ContentBlock::ToolResult(tr) => {
                // Concatenate any text-shaped tool_result fragments.
                let text: String = tr
                    .content()
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContentBlock::Text(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .collect();
                out.push(Content::ToolResult {
                    call_id: tr.tool_use_id().to_string(),
                    content: text,
                });
            }
            _ => {
                // Image / Document / GuardContent / ReasoningContent etc. —
                // not yet mapped onto canonical IR. Skipped, not fatal.
            }
        }
    }
    out
}

fn document_to_json(d: &Document) -> serde_json::Value {
    use aws_smithy_types::Number;
    match d {
        Document::Null => serde_json::Value::Null,
        Document::Bool(b) => serde_json::Value::Bool(*b),
        Document::Number(n) => match n {
            Number::NegInt(i) => serde_json::Value::from(*i),
            Number::PosInt(u) => serde_json::Value::from(*u),
            Number::Float(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
        },
        Document::String(s) => serde_json::Value::String(s.clone()),
        Document::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(document_to_json).collect())
        }
        Document::Object(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), document_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
    }
}

fn bedrock_stop_to_finish(s: &StopReason) -> Option<FinishReason> {
    // https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html
    // — `stopReason` valid values.
    match s {
        StopReason::EndTurn | StopReason::StopSequence => Some(FinishReason::Stop),
        StopReason::MaxTokens => Some(FinishReason::Length),
        StopReason::ToolUse => Some(FinishReason::ToolCalls),
        StopReason::GuardrailIntervened | StopReason::ContentFiltered => {
            Some(FinishReason::ContentFilter)
        }
        other => Some(FinishReason::Other(other.as_str().to_string())),
    }
}

// ===== streaming =====

fn event_to_parts(
    event: ConverseStreamOutput,
    tool_id_by_index: &mut std::collections::HashMap<i32, String>,
) -> Vec<StreamPart> {
    // ConverseStream emits these event types — see
    // https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStreamOutput.html
    match event {
        ConverseStreamOutput::MessageStart(_) => Vec::new(),
        ConverseStreamOutput::ContentBlockStart(ContentBlockStartEvent {
            content_block_index,
            start:
                Some(ContentBlockStart::ToolUse(ToolUseBlockStart {
                    tool_use_id, name, ..
                })),
            ..
        }) => {
            tool_id_by_index.insert(content_block_index, tool_use_id.clone());
            vec![StreamPart::ToolCallDelta {
                id: tool_use_id,
                name: Some(name),
                arguments: String::new(),
            }]
        }
        ConverseStreamOutput::ContentBlockStart(_) => Vec::new(),
        ConverseStreamOutput::ContentBlockDelta(ContentBlockDeltaEvent {
            content_block_index,
            delta,
            ..
        }) => match delta {
            Some(ContentBlockDelta::Text(text)) => vec![StreamPart::TextDelta { text }],
            Some(ContentBlockDelta::ToolUse(td)) => {
                let id = tool_id_by_index
                    .get(&content_block_index)
                    .cloned()
                    .unwrap_or_default();
                vec![StreamPart::ToolCallDelta {
                    id,
                    name: None,
                    arguments: td.input,
                }]
            }
            // ReasoningContent stream deltas (when models emit them) — map
            // onto canonical reasoning text. Other delta shapes are skipped
            // forward-compatibly. Ref:
            // https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ContentBlockDelta.html
            Some(ContentBlockDelta::ReasoningContent(
                aws_sdk_bedrockruntime::types::ReasoningContentBlockDelta::Text(text),
            )) => vec![StreamPart::ReasoningDelta { text }],
            _ => Vec::new(),
        },
        ConverseStreamOutput::ContentBlockStop(_) => Vec::new(),
        ConverseStreamOutput::MessageStop(MessageStopEvent { stop_reason, .. }) => {
            let reason = bedrock_stop_to_finish(&stop_reason).unwrap_or(FinishReason::Stop);
            vec![StreamPart::Finish { reason }]
        }
        ConverseStreamOutput::Metadata(ConverseStreamMetadataEvent { usage, .. }) => usage
            .map(|u| {
                vec![StreamPart::Usage {
                    usage: token_usage_to_canonical(&u),
                }]
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn token_usage_to_canonical(u: &bedrock::types::TokenUsage) -> Usage {
    // https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_TokenUsage.html
    Usage {
        prompt_tokens: u.input_tokens.max(0) as u64,
        completion_tokens: u.output_tokens.max(0) as u64,
        reasoning_tokens: 0,
        cache_read_tokens: u.cache_read_input_tokens.unwrap_or(0).max(0) as u64,
        cache_write_tokens: u.cache_write_input_tokens.unwrap_or(0).max(0) as u64,
    }
}

// ===== error mapping =====

/// Map an AWS SDK operation error to a `BitrouterError`, preserving the
/// upstream HTTP status where the SDK exposes it. Error type list:
/// <https://docs.aws.amazon.com/bedrock/latest/userguide/troubleshooting-api-error-codes.html>.
fn map_bedrock_err<E, R>(e: SdkError<E, R>) -> BitrouterError
where
    E: std::fmt::Display + std::fmt::Debug,
    R: std::fmt::Debug,
{
    match &e {
        SdkError::TimeoutError(_) => BitrouterError::UpstreamTimeout,
        SdkError::ServiceError(svc) => BitrouterError::Upstream {
            status: 502,
            message: format!("Bedrock service error: {}", svc.err()),
        },
        _ => BitrouterError::Upstream {
            status: 502,
            message: format!("Bedrock SDK error: {e:?}"),
        },
    }
}

/// Map a streaming-event recv error onto a `BitrouterError`. The event
/// receiver yields `SdkError<ConverseStreamOutputError, RawMessage>` rather
/// than a normal operation error, so it gets its own helper.
fn map_stream_err(
    e: SdkError<bedrock::types::error::ConverseStreamOutputError, RawMessage>,
) -> BitrouterError {
    BitrouterError::Upstream {
        status: 502,
        message: format!("Bedrock stream recv error: {e:?}"),
    }
}
