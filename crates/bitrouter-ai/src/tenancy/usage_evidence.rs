//! Versioned technical observations, independent of the frozen execution receipt.
//!
//! Frames are ordered observations, not additive totals. An omitted quantity was
//! absent at its source; an invalid quantity has a null value. Explicit zero is
//! retained. No response content, arbitrary metadata or credentials are accepted.

use super::{
    execution::{Outcome, Target},
    identity::{AttemptId, AuthorityRevision, ExecutionId, InstallationId, TenantId},
    validation::{Validate, ValidationError, bytes, nullable, require, unique},
};
use serde::{Deserialize, Serialize};

/// Maximum serialized evidence document size.
pub const MAX_BYTES: usize = 256 * 1024;
/// Maximum observations retained per execution.
pub const MAX_FRAMES: usize = 512;
/// Maximum observations retained per physical attempt.
pub const MAX_ATTEMPT_FRAMES: usize = 128;
/// Maximum quantities retained per observation.
pub const MAX_QUANTITIES: usize = 32;

#[cfg(feature = "schema")]
mod schema;

macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$doc:meta])* $variant:ident),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
        pub enum $name { $($(#[$doc])* $variant),+ }
    };
}
wire_enum! { /// Original protocol whose usage object was inspected.
    Protocol { /// Chat completions.
        ChatCompletions, /// Responses.
        Responses, /// Messages.
        Messages, /// Decisions.
        Decisions }
}
wire_enum! { /// Fixed source locations; never provider-controlled strings.
    Source { /// Nonstreaming response usage.
        Json, /// Rejected HTTP response usage.
        HttpError, /// Accepted chat usage chunk.
        ChatChunk, /// Accepted response terminal usage.
        ResponseTerminal, /// Accepted initial message usage.
        MessageStart, /// Accepted message update usage.
        MessageDelta }
}
wire_enum! { /// Arithmetic semantics of one source observation.
    Semantics { /// Replaces the named quantities; absent quantities remain absent.
        Snapshot, /// Adds the named quantities exactly once.
        Delta, /// No arithmetic interpretation is qualified.
        Unknown }
}
wire_enum! { /// Numeric units are encoded in each closed dimension name.
    Dimension { /// Native input tokens, before any normalization.
        InputTokens, /// Native output tokens.
        OutputTokens, /// Native total tokens.
        TotalTokens, /// Cache read tokens.
        CacheReadTokens, /// Cache creation tokens.
        CacheWriteTokens, /// Reasoning tokens.
        ReasoningTokens, /// Provider-reported web search requests.
        WebSearchRequests, /// Input audio tokens.
        InputAudioTokens, /// Output audio tokens.
        OutputAudioTokens, /// Input image tokens.
        InputImageTokens, /// Output image tokens.
        OutputImageTokens, /// Five-minute cache creation tokens.
        CacheWriteFiveMinuteTokens, /// One-hour cache creation tokens.
        CacheWriteOneHourTokens }
}
wire_enum! { /// Evidence retention, independent of invocation outcome.
    Capture { /// Capture was installed for this execution.
        Captured, /// Historical receipt with no evidence capture contract.
        NotCaptured, /// Capture was installed, but original observations were lost at recovery.
        Unavailable }
}
wire_enum! { /// Whether estimator provenance was captured by this contract.
    Estimation { /// No estimate is asserted by this document.
        NotCaptured, /// The installed stream finalizer did not use an estimator.
        NotUsed, /// The installed stream finalizer used the recorded estimator.
        Captured }
}
wire_enum! { /// Qualified estimator algorithm, independent of provider observations.
    EstimatorAlgorithm { /// Ceiling of Unicode scalar count divided by four.
        StreamCharacterCeiling }
}
wire_enum! { /// Exact coverage of this local estimate, excluding media and unobserved work.
    EstimateCoverage { /// Prompt textual payloads and observed text/reasoning deltas only.
        PromptTextAndObservedTextReasoning }
}
wire_enum! { /// Boundary at which the existing estimator was actually used.
    EstimateTrigger { /// Stream completion without usable provider counters.
        Completed, /// Consumer disconnect without usable provider counters.
        Disconnected }
}

/// An execution-stream estimate, not a per-attempt provider quantity or a total
/// across retries/children. Its numeric inputs never contain response content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Estimate {
    /// Qualified algorithm.
    pub algorithm: EstimatorAlgorithm,
    /// Algorithm revision, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    pub version: u8,
    /// Divisor used by this algorithm revision, exactly four.
    #[serde(deserialize_with = "super::number::integer")]
    pub characters_per_token: u8,
    /// Coverage excludes media, tool execution and unseen output.
    pub coverage: EstimateCoverage,
    /// Observed finalization boundary.
    pub trigger: EstimateTrigger,
    /// Unicode scalar count of seeded textual prompt payloads.
    #[serde(deserialize_with = "super::number::integer")]
    pub prompt_characters: u64,
    /// Unicode scalar count of observed text and reasoning deltas.
    #[serde(deserialize_with = "super::number::integer")]
    pub output_characters: u64,
    /// Estimated input tokens used by the finalizer.
    #[serde(deserialize_with = "super::number::integer")]
    pub input_tokens: u64,
    /// Estimated output tokens used by the finalizer.
    #[serde(deserialize_with = "super::number::integer")]
    pub output_tokens: u64,
}
impl Validate for Estimate {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.version == 1
                && self.characters_per_token == 4
                && self.prompt_characters <= AuthorityRevision::MAX
                && self.output_characters <= AuthorityRevision::MAX
                && self.input_tokens == self.prompt_characters.div_ceil(4)
                && self.output_tokens == self.output_characters.div_ceil(4),
            "stream estimator evidence",
        )
    }
}

/// One allowlisted source field. Absence is represented by no entry in the frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Quantity {
    /// Closed numeric dimension.
    pub dimension: Dimension,
    /// Original exact integer, or null for an invalid source value.
    #[serde(deserialize_with = "super::number::nullable_integer")]
    #[cfg_attr(
        feature = "schema",
        schemars(required, range(min = 0, max = 9007199254740991_u64))
    )]
    pub value: Option<u64>,
}

/// An original usage observation. No aggregation or contradiction repair occurs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Frame {
    /// Execution-local order, beginning at zero; retained frames form a prefix.
    #[serde(deserialize_with = "super::number::integer")]
    pub sequence: u64,
    /// Physical attempt that observed this frame.
    pub attempt_id: AttemptId,
    /// Original wire protocol.
    pub protocol: Protocol,
    /// Validated source location.
    pub source: Source,
    /// Arithmetic interpretation; message updates are snapshots, despite their name.
    pub semantics: Semantics,
    /// Only the named fields are described, never an assertion of a complete total.
    pub quantities: Vec<Quantity>,
    /// At least one unqualified source field or shape was present.
    pub unsupported: bool,
    /// A recognized source container or numeric field was malformed.
    pub invalid: bool,
}

/// Physical attempt identity and termination, including attempts with no usage frame.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AttemptEvidence {
    /// Matches the immutable receipt attempt.
    pub attempt_id: AttemptId,
    /// Matches the immutable receipt target.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(feature = "schema", schemars(required))]
    pub target: Option<Target>,
    /// Matches the immutable receipt outcome; unknown includes unresolved disconnects.
    pub outcome: Outcome,
    /// Whether the protocol completion boundary was observed.
    pub terminal_observed: bool,
}

/// Independently versioned evidence; adding fields to the receipt is forbidden.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionUsageEvidenceV1 {
    /// Exactly one for this schema.
    #[serde(deserialize_with = "super::number::integer")]
    pub schema_version: u8,
    /// Bound installation.
    pub installation_id: InstallationId,
    /// Bound tenant.
    pub tenant_id: TenantId,
    /// Bound execution.
    pub execution_id: ExecutionId,
    /// Distinguishes historical absence, retained capture and observations lost at recovery.
    pub capture: Capture,
    /// Extractor version, fixed at one for this protocol mapping.
    #[serde(deserialize_with = "super::number::integer")]
    pub extractor_version: u8,
    /// This version does not reconstruct estimates from canonical zero-filled usage.
    pub estimation: Estimation,
    /// Required null unless the existing stream estimator was actually used.
    #[serde(deserialize_with = "nullable")]
    pub estimate: Option<Estimate>,
    /// Exact owned parent execution, never inferred from correlation IDs.
    #[serde(deserialize_with = "nullable")]
    pub parent_execution_id: Option<ExecutionId>,
    /// Independently admitted owned children. Their usage must not be added to these frames.
    pub child_execution_ids: Vec<ExecutionId>,
    /// Attempts in receipt dispatch order, at most eight.
    pub attempts: Vec<AttemptEvidence>,
    /// Observations in execution order, with bounded prefix retention.
    pub frames: Vec<Frame>,
    /// Sticky once any frame cannot be retained.
    pub truncated: bool,
    /// Saturating exact count of discarded observations.
    #[serde(deserialize_with = "super::number::integer")]
    pub dropped_frames: u64,
}
impl Validate for ExecutionUsageEvidenceV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.schema_version == 1 && self.extractor_version == 1,
            "usage evidence version",
        )?;
        unique(
            &self
                .attempts
                .iter()
                .map(|a| &a.attempt_id)
                .collect::<Vec<_>>(),
            0,
            8,
        )?;
        require(
            self.frames.len() <= MAX_FRAMES && self.dropped_frames <= AuthorityRevision::MAX,
            "usage evidence bounds",
        )?;
        require(
            self.truncated == (self.dropped_frames > 0),
            "usage evidence truncation",
        )?;
        require(
            (self.estimation == Estimation::Captured) == self.estimate.is_some(),
            "estimator presence",
        )?;
        if let Some(estimate) = &self.estimate {
            estimate.validate()?;
        }
        unique(&self.child_execution_ids, 0, 64)?;
        require(
            self.parent_execution_id.as_ref() != Some(&self.execution_id)
                && !self.child_execution_ids.contains(&self.execution_id)
                && self
                    .parent_execution_id
                    .as_ref()
                    .is_none_or(|parent| !self.child_execution_ids.contains(parent)),
            "execution linkage",
        )?;
        if self.capture != Capture::Captured {
            require(
                self.attempts.is_empty()
                    && self.frames.is_empty()
                    && !self.truncated
                    && self.estimation == Estimation::NotCaptured
                    && self.parent_execution_id.is_none()
                    && self.child_execution_ids.is_empty(),
                "absent evidence cannot assert observations",
            )?;
        }
        for attempt in &self.attempts {
            if let Some(target) = &attempt.target {
                target.validate()?;
            }
            require(
                !attempt.terminal_observed
                    || matches!(attempt.outcome, Outcome::Completed | Outcome::Failed),
                "usage terminal outcome",
            )?;
            require(
                self.frames
                    .iter()
                    .filter(|f| f.attempt_id == attempt.attempt_id)
                    .count()
                    <= MAX_ATTEMPT_FRAMES,
                "attempt frame bound",
            )?;
        }
        for (index, frame) in self.frames.iter().enumerate() {
            require(
                frame.sequence == index as u64
                    && self
                        .attempts
                        .iter()
                        .any(|a| a.attempt_id == frame.attempt_id),
                "usage frame identity/order",
            )?;
            unique(
                &frame
                    .quantities
                    .iter()
                    .map(|q| q.dimension)
                    .collect::<Vec<_>>(),
                0,
                MAX_QUANTITIES,
            )?;
            require(
                frame
                    .quantities
                    .iter()
                    .all(|q| q.value.is_none_or(|v| v <= AuthorityRevision::MAX)),
                "quantity range",
            )?;
            require(
                frame.invalid || frame.quantities.iter().all(|q| q.value.is_some()),
                "invalid quantity marker",
            )?;
            let source_matches = matches!(
                (frame.protocol, frame.source),
                (_, Source::Json | Source::HttpError)
                    | (Protocol::ChatCompletions, Source::ChatChunk)
                    | (Protocol::Responses, Source::ResponseTerminal)
                    | (
                        Protocol::Messages,
                        Source::MessageStart | Source::MessageDelta
                    )
            );
            require(source_matches, "usage source protocol")?;
            require(
                frame.semantics
                    == if frame.protocol == Protocol::Decisions {
                        Semantics::Unknown
                    } else {
                        Semantics::Snapshot
                    },
                "extractor semantics",
            )?;
        }
        bytes(self, MAX_BYTES)
    }
}

/// Separate discovery prevents widening the frozen tenancy capabilities DTO.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageEvidenceCapabilitiesV1 {
    /// Contract version, exactly one.
    pub schema_version: u8,
    /// Bound installation.
    pub installation_id: InstallationId,
    /// Maximum encoded document bytes.
    pub max_bytes: u64,
    /// Maximum frames per execution.
    pub max_frames: u64,
    /// Maximum frames per attempt.
    pub max_attempt_frames: u64,
    /// Maximum quantities per frame.
    pub max_quantities: u64,
    /// Whether the existing stream estimator is captured with algorithm/version and coverage.
    pub estimator_capture: bool,
    /// Whether this build links independently admitted Decisions children.
    pub child_linkage: bool,
    /// False: this contract does not qualify a generic server tool loop.
    pub server_tool_capture: bool,
}
impl UsageEvidenceCapabilitiesV1 {
    /// Describe the bounded extractor supported by version one.
    pub fn new(installation_id: InstallationId) -> Self {
        Self {
            schema_version: 1,
            installation_id,
            max_bytes: MAX_BYTES as u64,
            max_frames: MAX_FRAMES as u64,
            max_attempt_frames: MAX_ATTEMPT_FRAMES as u64,
            max_quantities: MAX_QUANTITIES as u64,
            estimator_capture: true,
            child_linkage: false,
            server_tool_capture: false,
        }
    }
}
impl Validate for UsageEvidenceCapabilitiesV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.schema_version == 1
                && self.max_bytes == MAX_BYTES as u64
                && self.max_frames == MAX_FRAMES as u64
                && self.max_attempt_frames == MAX_ATTEMPT_FRAMES as u64
                && self.max_quantities == MAX_QUANTITIES as u64
                && self.estimator_capture
                && !self.server_tool_capture,
            "usage evidence capabilities",
        )
    }
}
