//! Gateway conversion admission and projection rules.

use crate::gateway::model::error::{ModelError, Result};
use bitrouter_ai::conversion::{
    ConversionDisposition, ConversionEffect, ConversionIssue, ConversionLocation, ConversionReason,
    ConversionReport, ConversionStage,
};
use bitrouter_ai::types::{
    ApiProtocol, Content, Prompt, ResponseFormat, Role, Tool, ToolResultContentPart,
    ToolResultOutput, provider_namespace,
};

/// Gateway admission and diagnostic construction for public conversion reports.
pub trait ReportAdmission {
    fn push_equivalent(
        &mut self,
        protocol: &ApiProtocol,
        location: ConversionLocation,
        reason: ConversionReason,
    );
    fn require_admitted(&self) -> Result<()>;
    fn push_ingress(
        &mut self,
        protocol: &ApiProtocol,
        location: ConversionLocation,
        reason: ConversionReason,
        effect: ConversionEffect,
    );
    fn push_projection(
        &mut self,
        protocol: &ApiProtocol,
        location: ConversionLocation,
        reason: ConversionReason,
        effect: ConversionEffect,
    );
    fn is_output_failure(&self) -> bool;
}

impl ReportAdmission for ConversionReport {
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
    fn require_admitted(&self) -> Result<()> {
        if self.issues.is_empty() {
            Ok(())
        } else {
            Err(ModelError::Incompatible {
                report: self.clone(),
            })
        }
    }

    /// Record a content-free refusal before constructing a canonical Prompt.
    fn push_ingress(
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
    fn is_output_failure(&self) -> bool {
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
            if let Content::Text {
                provider_metadata, ..
            } = content
                && provider_metadata
                    .get("openai")
                    .and_then(|value| value.get("messagePhase"))
                    .is_some()
                && (*protocol != ApiProtocol::Responses || entry.role != Role::Assistant)
            {
                report.push_projection(
                    protocol,
                    location,
                    ConversionReason::HistoryContentUnrepresentable,
                    ConversionEffect::TaskSemantics,
                );
                continue;
            }
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
                if crate::gateway::model::protocol::messages::render_reasoning_block(
                    text,
                    provider_metadata,
                )
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
                        && crate::gateway::model::protocol::messages::mcp_server_name(
                            provider_metadata,
                        )
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
