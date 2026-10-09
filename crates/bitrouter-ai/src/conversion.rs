//! Content-free conversion admission diagnostics shared by direct and routed calls.
//!
//! These rules cover initial ingress and structural/selected attribute projection
//! losses, native reasoning and structured outputs. Remaining codec effects are
//! not certified lossless.

use serde::{Deserialize, Serialize};

use crate::error::{ModelError, Result};
use crate::types::{
    ApiProtocol, Content, Prompt, ResponseFormat, Role, Tool, ToolResultContentPart,
    ToolResultOutput, provider_namespace,
};

/// Wire category without caller-controlled custom names or target credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversionProtocol {
    /// Native Decisions, which has no generative projection.
    Decisions,
    /// OpenAI Chat Completions.
    ChatCompletions,
    /// OpenAI Responses.
    Responses,
    /// Anthropic Messages.
    Messages,
    /// Historical diagnostic provenance only; no executable native codec.
    GenerateContent,
    /// An explicitly registered custom wire.
    Custom,
}

impl From<&ApiProtocol> for ConversionProtocol {
    fn from(protocol: &ApiProtocol) -> Self {
        match protocol {
            ApiProtocol::ChatCompletions => Self::ChatCompletions,
            ApiProtocol::Responses => Self::Responses,
            ApiProtocol::Decisions => Self::Decisions,
            ApiProtocol::Messages => Self::Messages,

            ApiProtocol::Custom(_) => Self::Custom,
        }
    }
}

/// Boundary at which a conversion was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversionStage {
    /// Original wire input to canonical request semantics.
    RequestIngress,
    /// Canonical history to one selected upstream request.
    RequestProjection,
    /// Canonical result to the client's response body.
    ResponseEncoding,
    /// Canonical reasoning terminal to the client's stream.
    StreamEncoding,
}

/// A bounded reason; never contains prompt text or opaque native material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversionReason {
    /// The selected wire implements a different model operation.
    OperationUnsupported,
    /// No canonical input mapping exists for this item.
    UnclassifiedInputItem,
    /// No canonical input mapping exists for this content block or value.
    UnclassifiedInputContent,
    /// The current canonical input slot cannot retain the recognized payload.
    InputContentUnrepresentable,
    /// A reasoning input has no required continuity token.
    ReasoningContinuityMissing,
    /// Preserving a native item does not prove selected-target replay authority.
    NativeReasoningAuthorityUnproven,
    /// This wire cannot preserve the retained Responses reasoning item.
    NativeReasoningUnrepresentable,
    /// A custom wire has no classified native reasoning compatibility rule.
    NativeReasoningCompatibilityUnknown,
    /// A selected adapter does not implement structured outputs.
    ResponseFormatUnsupported,
    /// This request renderer has no slot for a canonical history block.
    HistoryContentUnrepresentable,
    /// A history source/citation omission has no nonessential classification.
    HistorySourceOmissionUnclassified,
    /// Provider execution or runtime server identity would be lost or omitted.
    ProviderToolHistoryUnrepresentable,
    /// Custom-wire tool/history compatibility has not been classified.
    HistoryCompatibilityUnknown,
    /// A selected renderer would drop a tool result's media or file reference.
    ToolResultPartUnrepresentable,
    /// The request wire has no faithful approval representation.
    ApprovalUnrepresentable,
    /// A represented native continuity token would be omitted on this wire.
    NativeContinuityUnrepresentable,
    /// A native approval response has no slot for the caller's reason.
    ApprovalReasonUnrepresentable,
    /// This request wire omits provider-defined tool declarations.
    ProviderToolDefinitionUnrepresentable,
    /// A native provider-tool serializer would discard non-object arguments.
    ProviderToolArgumentsUnrepresentable,
    /// Foreign/native provider-tool translation is not implemented for this wire.
    ProviderToolDefinitionCompatibilityUnknown,
    /// A tagged denial would disappear without its denied approval response.
    ApprovalDenialUnpaired,
    /// The target omits an explicitly supplied function-tool strict flag.
    FunctionStrictUnrepresentable,
    /// The actual tool-schema rewrite has no proven equivalent representation.
    ToolSchemaProjectionUnclassified,
    /// A structured-output name or description has no target slot.
    ResponseFormatMetadataUnrepresentable,
    /// An explicit structured-output strict flag has no faithful target slot.
    ResponseFormatStrictUnrepresentable,
    /// A filename omission has not been classified as nonessential.
    FileNameOmissionUnclassified,
    /// An error or execution-denied result would become an ordinary result.
    ToolResultStatusUnrepresentable,
    /// A native approval response cannot carry a suppressed result's denial reason.
    DenialReasonUnrepresentable,
    /// A structured argument string would be replaced by an empty object.
    ToolArgumentsUnrepresentable,
    /// A result kind or JSON wrapper rewrite has no equivalent classification.
    ToolResultShapeProjectionUnclassified,
    /// Separate content parts would collapse into one textual slot.
    ContentBoundaryUnclassified,
    /// Content would move relative to another original block.
    ContentOrderUnclassified,
    /// An input attribute is discarded before canonical data exists.
    InputAttributeUnclassified,
    /// Canonical JSON is represented by its value-preserving JSON encoding.
    ToolResultJsonEncoding,
    /// A bounded equivalent Gemini single-type/nullable schema rewrite.
    GeminiSchemaNormalization,
}

/// Classified effect of a detected conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversionEffect {
    /// Representation changes while the classified source semantics are retained.
    EquivalentRepresentation,
    /// Replay authority is not established; conversion policy cannot waive it.
    ReplayAuthority,
    /// Required request or result semantics would be lost.
    TaskSemantics,
    /// The input mapping or selected-wire compatibility is unclassified.
    Unknown,
}

/// Required handling of this diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversionDisposition {
    /// This classified effect is allowed; overall admission still checks refusals.
    Allow,
    /// Reject ingress before lossy canonicalization or route selection.
    RejectRequest,
    /// Native replay cannot proceed without a separate authority proof.
    RejectReplay,
    /// Skip this route before attempting provider execution.
    ExcludeTarget,
    /// Fail client encoding while retaining the actual upstream attempt.
    FailOutput,
}

/// Structural location with no transcript or provider-controlled identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "field", rename_all = "snake_case")]
pub enum ConversionLocation {
    /// Whole-call semantic operation.
    Operation,
    /// A Responses input array item.
    InputItem {
        /// Zero-based original input index.
        item: usize,
    },
    /// A Responses message input content block.
    InputContent {
        /// Zero-based original input index.
        item: usize,
        /// Zero-based content block index.
        block: usize,
    },
    /// A Responses message's complete content value.
    InputContentValue {
        /// Zero-based original input index.
        item: usize,
    },
    /// A Responses tool output content array member.
    InputToolResultContent {
        /// Zero-based original input index.
        item: usize,
        /// Zero-based output block index.
        block: usize,
    },
    /// A message's complete content value on the other three wires.
    MessageContentValue {
        /// Zero-based original message/content-turn index.
        message: usize,
    },
    /// A tool-result content part: original Messages wire coordinates on ingress,
    /// original canonical message/block/part coordinates on projection.
    ToolResultContent {
        /// Zero-based original message index.
        message: usize,
        /// Zero-based enclosing tool-result block index.
        block: usize,
        /// Zero-based nested result content index.
        part: usize,
    },
    /// An unsupported complete Messages system value.
    SystemContentValue,
    /// A Messages system or Gemini systemInstruction content block.
    SystemContent {
        /// Zero-based original system content index.
        block: usize,
    },
    /// A message block; ingress indices refer to the original wire array,
    /// projection indices to the original canonical request.
    MessageContent {
        /// Zero-based message index.
        message: usize,
        /// Zero-based content index within that message.
        block: usize,
    },
    /// A canonical result block.
    OutputContent {
        /// Zero-based result content index.
        block: usize,
    },
    /// A native reasoning terminal event; native item IDs are intentionally absent.
    StreamReasoning,
    /// A tool-call stream event; opaque tool IDs are intentionally absent.
    StreamToolCall,
    /// A canonical provider-defined tool declaration.
    ToolDefinition {
        /// Zero-based original tool index.
        tool: usize,
    },
    /// The canonical structured-output requirement.
    ResponseFormat,
    /// Target-specific generation controls in the source request.
    GenerationOptions,
}

/// One categorical conversion diagnostic, suitable for trusted observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversionIssue {
    /// Boundary where the effect was detected.
    pub stage: ConversionStage,
    /// Wire category at this boundary. The router observes the concrete target separately.
    pub protocol: ConversionProtocol,
    /// Structural source location.
    pub location: ConversionLocation,
    /// Bounded reason for the detected effect.
    pub reason: ConversionReason,
    /// Classified equivalent, task, replay-authority or unknown effect.
    pub effect: ConversionEffect,
    /// Required handling; no blanket degradation override exists.
    pub disposition: ConversionDisposition,
}

/// Refusals and classified equivalent effects found by the initial rules.
///
/// No refusals means these rules pass, not that all fidelity/authority is proven.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversionReport {
    /// Refused conversions, in source order.
    pub issues: Vec<ConversionIssue>,
    /// Classified equivalent effects, independently of overall candidate admission.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub admitted: Vec<ConversionIssue>,
}

impl ConversionReport {
    fn push_equivalent(
        &mut self,
        protocol: &ApiProtocol,
        location: ConversionLocation,
        reason: ConversionReason,
    ) {
        self.admitted.push(ConversionIssue {
            stage: ConversionStage::RequestProjection,
            protocol: protocol.into(),
            location,
            reason,
            effect: ConversionEffect::EquivalentRepresentation,
            disposition: ConversionDisposition::Allow,
        });
    }
    /// Reject before dispatch or encoding when any rule has refused conversion.
    pub fn require_admitted(&self) -> Result<()> {
        if self.issues.is_empty() {
            Ok(())
        } else {
            Err(ModelError::Incompatible {
                report: self.clone(),
            })
        }
    }

    /// Record a content-free refusal before constructing a canonical Prompt.
    pub(crate) fn push_ingress(
        &mut self,
        protocol: &ApiProtocol,
        location: ConversionLocation,
        reason: ConversionReason,
        effect: ConversionEffect,
    ) {
        self.issues.push(ConversionIssue {
            stage: ConversionStage::RequestIngress,
            protocol: protocol.into(),
            location,
            reason,
            effect,
            disposition: if effect == ConversionEffect::ReplayAuthority {
                ConversionDisposition::RejectReplay
            } else {
                ConversionDisposition::RejectRequest
            },
        });
    }

    fn push_projection(
        &mut self,
        protocol: &ApiProtocol,
        location: ConversionLocation,
        reason: ConversionReason,
        effect: ConversionEffect,
    ) {
        self.issues.push(ConversionIssue {
            stage: ConversionStage::RequestProjection,
            protocol: protocol.into(),
            location,
            reason,
            effect,
            disposition: if effect == ConversionEffect::ReplayAuthority {
                ConversionDisposition::RejectReplay
            } else {
                ConversionDisposition::ExcludeTarget
            },
        });
    }

    /// Whether this report concerns output from an already executed provider.
    pub fn is_output_failure(&self) -> bool {
        self.issues.iter().any(|issue| {
            matches!(
                issue.stage,
                ConversionStage::ResponseEncoding | ConversionStage::StreamEncoding
            )
        })
    }
}

/// Check initial request projection losses without modifying source data or
/// resolving credentials. Passing these rules is not a replay-authority proof
/// or a certification of remaining codec or schema/model behavior.
pub fn request_admission(prompt: &Prompt, protocol: &ApiProtocol) -> ConversionReport {
    let mut report = ConversionReport::default();
    if *protocol == ApiProtocol::Decisions {
        report.push_projection(
            protocol,
            ConversionLocation::Operation,
            ConversionReason::OperationUnsupported,
            ConversionEffect::TaskSemantics,
        );
        return report;
    }
    for (tool, definition) in prompt.tools.iter().enumerate() {
        if let Tool::Function { strict, .. } = definition
            && strict.is_some()
            && matches!(protocol, ApiProtocol::Messages)
        {
            report.push_projection(
                protocol,
                ConversionLocation::ToolDefinition { tool },
                ConversionReason::FunctionStrictUnrepresentable,
                ConversionEffect::TaskSemantics,
            );
        }
        let Tool::ProviderDefined { id, args, .. } = definition else {
            continue;
        };
        let family = match protocol {
            ApiProtocol::Responses => Some("openai"),
            ApiProtocol::Messages => Some("anthropic"),
            ApiProtocol::ChatCompletions | ApiProtocol::Custom(_) | ApiProtocol::Decisions => None,
        };
        let refusal = if *protocol == ApiProtocol::ChatCompletions {
            Some((
                ConversionReason::ProviderToolDefinitionUnrepresentable,
                ConversionEffect::TaskSemantics,
            ))
        } else if family.is_some_and(|family| {
            id.split_once('.')
                .is_some_and(|(owner, name)| owner == family && !name.is_empty())
        }) {
            if args.is_object() {
                None
            } else {
                Some((
                    ConversionReason::ProviderToolArgumentsUnrepresentable,
                    ConversionEffect::TaskSemantics,
                ))
            }
        } else {
            Some((
                ConversionReason::ProviderToolDefinitionCompatibilityUnknown,
                ConversionEffect::Unknown,
            ))
        };
        if let Some((reason, effect)) = refusal {
            report.push_projection(
                protocol,
                ConversionLocation::ToolDefinition { tool },
                reason,
                effect,
            );
        }
    }
    if matches!(protocol, ApiProtocol::Messages)
        && let Some(ResponseFormat::JsonSchema {
            name,
            description,
            strict,
            ..
        }) = &prompt.response_format
    {
        if name.is_some() || description.is_some() {
            report.push_projection(
                protocol,
                ConversionLocation::ResponseFormat,
                ConversionReason::ResponseFormatMetadataUnrepresentable,
                ConversionEffect::Unknown,
            );
        }
        if strict.is_some() {
            report.push_projection(
                protocol,
                ConversionLocation::ResponseFormat,
                ConversionReason::ResponseFormatStrictUnrepresentable,
                ConversionEffect::TaskSemantics,
            );
        }
    }
    for (message, entry) in prompt.messages.iter().enumerate() {
        let mut chat_call_seen = false;
        let mut chat_non_reasoning_seen = false;
        let mut chat_reasoning_seen = false;
        for (block, content) in entry.content.iter().enumerate() {
            let location = ConversionLocation::MessageContent { message, block };
            if let Content::Reasoning {
                native: Some(_), ..
            } = content
            {
                let (reason, effect) = match protocol {
                    ApiProtocol::Responses => (
                        ConversionReason::NativeReasoningAuthorityUnproven,
                        ConversionEffect::ReplayAuthority,
                    ),
                    ApiProtocol::Custom(_) => (
                        ConversionReason::NativeReasoningCompatibilityUnknown,
                        ConversionEffect::Unknown,
                    ),
                    ApiProtocol::ChatCompletions
                    | ApiProtocol::Messages
                    | ApiProtocol::Decisions => (
                        ConversionReason::NativeReasoningUnrepresentable,
                        ConversionEffect::TaskSemantics,
                    ),
                };
                report.push_projection(protocol, location, reason, effect);
                continue;
            }
            let continuity = match content {
                Content::Reasoning {
                    provider_metadata, ..
                }
                | Content::ToolCall {
                    provider_metadata, ..
                } => {
                    let anthropic =
                        provider_namespace(provider_metadata, "anthropic").is_some_and(|fields| {
                            fields.contains_key("signature")
                                || fields
                                    .get("redactedThinking")
                                    .and_then(serde_json::Value::as_bool)
                                    == Some(true)
                        });
                    let google = provider_namespace(provider_metadata, "google")
                        .is_some_and(|fields| fields.contains_key("thoughtSignature"));
                    (anthropic && *protocol != ApiProtocol::Messages)
                        || (google
                            && !(*protocol == ApiProtocol::ChatCompletions
                                && matches!(content, Content::ToolCall { .. })))
                }
                _ => false,
            };
            if continuity {
                report.push_projection(
                    protocol,
                    location,
                    ConversionReason::NativeContinuityUnrepresentable,
                    ConversionEffect::ReplayAuthority,
                );
                continue;
            }
            let refusal = history_refusal(prompt, entry.role, content, protocol);
            if let Some((reason, effect)) = refusal {
                report.push_projection(protocol, location, reason, effect);
                continue;
            }
            if matches!(
                protocol,
                ApiProtocol::ChatCompletions | ApiProtocol::Responses | ApiProtocol::Messages
            ) && matches!(
                content,
                Content::ToolResult {
                    output: ToolResultOutput::Json { .. } | ToolResultOutput::ErrorJson { .. },
                    dynamic: false,
                    ..
                }
            ) {
                report.push_equivalent(
                    protocol,
                    location,
                    ConversionReason::ToolResultJsonEncoding,
                );
            }
            if *protocol == ApiProtocol::ChatCompletions {
                let order_refusal = match content {
                    Content::Reasoning { .. } if chat_reasoning_seen => {
                        Some(ConversionReason::ContentBoundaryUnclassified)
                    }
                    Content::Reasoning { .. } if chat_non_reasoning_seen => {
                        Some(ConversionReason::ContentOrderUnclassified)
                    }
                    Content::Text { .. } | Content::File { .. } if chat_call_seen => {
                        Some(ConversionReason::ContentOrderUnclassified)
                    }
                    _ => None,
                };
                if let Some(reason) = order_refusal {
                    report.push_projection(protocol, location, reason, ConversionEffect::Unknown);
                }
                if matches!(content, Content::Reasoning { .. }) {
                    chat_reasoning_seen = true;
                } else {
                    chat_non_reasoning_seen = true;
                    if matches!(content, Content::ToolCall { .. }) {
                        chat_call_seen = true;
                    }
                }
            }
            if let Content::File {
                media_type,
                filename: Some(_),
                ..
            } = content
            {
                let retains_name = match protocol {
                    ApiProtocol::ChatCompletions => {
                        !media_type.starts_with("image/") && !media_type.starts_with("audio/")
                    }
                    ApiProtocol::Responses => !media_type.starts_with("image/"),
                    _ => false,
                };
                if !retains_name {
                    report.push_projection(
                        protocol,
                        location,
                        ConversionReason::FileNameOmissionUnclassified,
                        ConversionEffect::Unknown,
                    );
                }
            }
            if let Content::ToolResult {
                output: ToolResultOutput::Content { value },
                ..
            } = content
            {
                for (part, value) in value.iter().enumerate() {
                    let refused = match protocol {
                        ApiProtocol::Decisions => true,
                        ApiProtocol::Responses => false,
                        ApiProtocol::ChatCompletions => {
                            matches!(value, ToolResultContentPart::FileId { .. })
                        }
                        ApiProtocol::Messages => match value {
                            ToolResultContentPart::Text { .. } => false,
                            ToolResultContentPart::Media { media_type, .. } => {
                                !media_type.starts_with("image/")
                            }
                            ToolResultContentPart::FileId { .. } => true,
                        },
                        ApiProtocol::Custom(_) => {
                            !matches!(value, ToolResultContentPart::Text { .. })
                        }
                    };
                    if refused {
                        let (reason, effect) = if matches!(protocol, ApiProtocol::Custom(_)) {
                            (
                                ConversionReason::HistoryCompatibilityUnknown,
                                ConversionEffect::Unknown,
                            )
                        } else {
                            (
                                ConversionReason::ToolResultPartUnrepresentable,
                                ConversionEffect::TaskSemantics,
                            )
                        };
                        report.push_projection(
                            protocol,
                            ConversionLocation::ToolResultContent {
                                message,
                                block,
                                part,
                            },
                            reason,
                            effect,
                        );
                    }
                }
            }
        }
    }
    report
}

fn history_refusal(
    prompt: &Prompt,
    role: Role,
    content: &Content,
    protocol: &ApiProtocol,
) -> Option<(ConversionReason, ConversionEffect)> {
    use ConversionEffect::{ReplayAuthority, TaskSemantics, Unknown};
    use ConversionReason as Reason;
    if let Content::ToolCall { arguments, .. } = content
        && matches!(protocol, ApiProtocol::Messages)
        && serde_json::from_str::<serde_json::Value>(arguments).is_err()
    {
        return Some((Reason::ToolArgumentsUnrepresentable, TaskSemantics));
    }
    // Source/citation omission is not classified as harmless by these rules.
    if matches!(content, Content::Source { .. }) {
        return Some((Reason::HistorySourceOmissionUnclassified, Unknown));
    }
    if matches!(content, Content::ToolApprovalRequest { .. }) {
        return Some(if matches!(protocol, ApiProtocol::Custom(_)) {
            (Reason::HistoryCompatibilityUnknown, Unknown)
        } else {
            (Reason::ApprovalUnrepresentable, TaskSemantics)
        });
    }
    if let Content::ToolApprovalResponse { reason, .. } = content {
        return match protocol {
            ApiProtocol::Responses if reason.is_none() => None,
            ApiProtocol::Responses => Some((Reason::ApprovalReasonUnrepresentable, TaskSemantics)),
            ApiProtocol::Custom(_) => Some((Reason::HistoryCompatibilityUnknown, Unknown)),
            _ => Some((Reason::ApprovalUnrepresentable, TaskSemantics)),
        };
    }
    if role == Role::Tool
        && matches!(
            protocol,
            ApiProtocol::ChatCompletions | ApiProtocol::Messages
        )
        && !matches!(content, Content::ToolResult { .. })
    {
        return Some((Reason::HistoryContentUnrepresentable, TaskSemantics));
    }
    match content {
        Content::Reasoning {
            text,
            provider_metadata,
            ..
        } => match protocol {
            ApiProtocol::Responses => Some((Reason::HistoryContentUnrepresentable, TaskSemantics)),
            ApiProtocol::Messages
                if crate::protocol::messages::render_reasoning_block(text, provider_metadata)
                    .is_none() =>
            {
                Some((Reason::ReasoningContinuityMissing, ReplayAuthority))
            }
            ApiProtocol::Custom(_) => Some((Reason::HistoryCompatibilityUnknown, Unknown)),
            _ => None,
        },
        Content::ToolCall {
            provider_executed,
            dynamic,
            provider_metadata,
            ..
        } if *provider_executed || *dynamic => match protocol {
            ApiProtocol::Messages
                if !*dynamic
                    || (*provider_executed
                        && crate::protocol::messages::mcp_server_name(provider_metadata)
                            .is_some()) =>
            {
                None
            }
            ApiProtocol::Custom(_) => Some((Reason::HistoryCompatibilityUnknown, Unknown)),
            _ => Some((Reason::ProviderToolHistoryUnrepresentable, TaskSemantics)),
        },
        Content::ToolResult {
            dynamic,
            output,
            provider_metadata,
            ..
        } => {
            if *dynamic {
                return match protocol {
                    ApiProtocol::Messages
                        if role != Role::Tool
                            && matches!(
                                output,
                                ToolResultOutput::Json { .. } | ToolResultOutput::ErrorJson { .. }
                            ) =>
                    {
                        None
                    }
                    ApiProtocol::Custom(_) => Some((Reason::HistoryCompatibilityUnknown, Unknown)),
                    _ => Some((Reason::ProviderToolHistoryUnrepresentable, TaskSemantics)),
                };
            }
            if role != Role::Tool
                && matches!(
                    protocol,
                    ApiProtocol::ChatCompletions | ApiProtocol::Messages
                )
            {
                return Some((Reason::HistoryContentUnrepresentable, TaskSemantics));
            }
            if *protocol == ApiProtocol::Responses
                && matches!(output, ToolResultOutput::ExecutionDenied { .. })
                && let Some(fields) = provider_namespace(provider_metadata, "openai")
                && fields.contains_key("approvalId")
            {
                let id = fields.get("approvalId").and_then(serde_json::Value::as_str);
                let paired = id.is_some_and(|id| prompt.messages.iter().flat_map(|entry| &entry.content).any(|part| matches!(part, Content::ToolApprovalResponse { approval_id, approved: false, .. } if approval_id == id)));
                if !paired {
                    return Some((Reason::ApprovalDenialUnpaired, ReplayAuthority));
                }
                if matches!(
                    output,
                    ToolResultOutput::ExecutionDenied { reason: Some(_) }
                ) {
                    return Some((Reason::DenialReasonUnrepresentable, TaskSemantics));
                }
                return None;
            }
            if matches!(output, ToolResultOutput::ExecutionDenied { .. })
                || (output.is_error() && *protocol != ApiProtocol::Messages)
            {
                return Some(if matches!(protocol, ApiProtocol::Custom(_)) {
                    (Reason::HistoryCompatibilityUnknown, Unknown)
                } else {
                    (Reason::ToolResultStatusUnrepresentable, TaskSemantics)
                });
            }
            let changes_shape = match protocol {
                ApiProtocol::Decisions => true,
                // These wire slots carry canonical JSON as its JSON encoding;
                // serialization preserves the complete value. Status is checked
                // independently above, not waived by that representation.
                ApiProtocol::ChatCompletions
                | ApiProtocol::Responses
                | ApiProtocol::Messages
                | ApiProtocol::Custom(_) => false,
            };
            if changes_shape {
                return Some((Reason::ToolResultShapeProjectionUnclassified, Unknown));
            }
            None
        }
        _ => None,
    }
}
