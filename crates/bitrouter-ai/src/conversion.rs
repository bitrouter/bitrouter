//! Public conversion diagnostics. Admission rules and conversions belong to the gateway.

use serde::{Deserialize, Serialize};

use crate::types::ApiProtocol;

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
