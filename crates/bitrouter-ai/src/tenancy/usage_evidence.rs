//! Bounded technical observations, independent of the frozen execution receipt.
//!
//! Frames are ordered observations, not additive totals. An omitted quantity was
//! absent at its source; an invalid quantity has a null value. Explicit zero is
//! retained. No response content, arbitrary metadata or credentials are accepted.

use super::{
    execution::{Dispatch, ExecutionReceipt, Outcome, Target},
    identity::{AttemptId, AuthorityRevision, ExecutionId, InstallationId, TenantId},
    validation::{Validate, ValidationError, bytes, nullable, require, unique, write_json},
};
use serde::{Deserialize, Serialize};

/// Maximum serialized evidence document size.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Maximum observations retained per execution.
pub const MAX_FRAMES: usize = 4096;
/// Pure reservation bounds computed from the accepted count, not actual sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvidenceBudget {
    /// Total encoded bytes: 256 KiB per block of eight accepted candidates.
    pub max_bytes: usize,
    /// Frame array bytes: 192 KiB per block of eight accepted candidates.
    pub max_frame_bytes: usize,
    /// Reserved identity/terminal bytes: 64 KiB per block of eight candidates.
    pub reserved_bytes: usize,
    /// Frames per execution: 512 per block of eight accepted candidates.
    pub max_frames: usize,
}
impl EvidenceBudget {
    /// Fixed strict-dispatch reservation, also one ordered block of eight.
    pub const STRICT: Self = Self {
        max_bytes: 256 * 1024,
        max_frame_bytes: 192 * 1024,
        reserved_bytes: 64 * 1024,
        max_frames: 512,
    };

    /// Compute the evidence reservation for the whole accepted selection.
    ///
    /// # Errors
    /// Rejects counts outside 1–64, including an empty selection.
    pub fn for_attempts(accepted_count: usize) -> Result<Self, ValidationError> {
        require(
            (1..=super::ordered::MAX_ATTEMPTS).contains(&accepted_count),
            "evidence accepted count",
        )?;
        let blocks = accepted_count.div_ceil(8);
        Ok(Self {
            max_bytes: Self::STRICT.max_bytes * blocks,
            max_frame_bytes: Self::STRICT.max_frame_bytes * blocks,
            reserved_bytes: Self::STRICT.reserved_bytes * blocks,
            max_frames: Self::STRICT.max_frames * blocks,
        })
    }
}

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
        Decisions, /// Private ordered native GenerateContent. Ordered dispatch only.
        GenerateContent }
}
wire_enum! { /// Fixed source locations; never provider-controlled strings.
    Source { /// Nonstreaming response usage.
        Json, /// Rejected HTTP response usage.
        HttpError, /// Accepted chat usage chunk.
        ChatChunk, /// Accepted response terminal usage.
        ResponseTerminal, /// Accepted initial message usage.
        MessageStart, /// Accepted message update usage.
        MessageDelta, /// Accepted native GenerateContent usageMetadata snapshot. Ordered dispatch only.
        GenerateContentChunk }
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
        NotCaptured, /// The stream finalizer or successful native JSON executor affirmatively observed no estimator use.
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
    /// Chat metadata exceptions are exact: advisory root cost, zero-only
    /// `prompt_tokens_details.created_cache_tokens`, and null-only
    /// `prompt_tokens_details.multimodal_tokens`. None adds a quantity.
    pub unsupported: bool,
    /// A recognized source container or numeric field was malformed, including
    /// a present Chat advisory cost that is not a finite nonnegative JSON number.
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

/// Original observations bound to the immutable receipt and its dispatch.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionUsageEvidence {
    /// Exact dispatch accepted by the corresponding receipt.
    pub dispatch: Dispatch,
    /// Bound installation.
    pub installation_id: InstallationId,
    /// Bound tenant.
    pub tenant_id: TenantId,
    /// Bound execution.
    pub execution_id: ExecutionId,
    /// Distinguishes historical absence, retained capture and observations lost at recovery.
    pub capture: Capture,
    /// Qualified mapping discriminator: two for native ingress or observations, one otherwise.
    #[serde(deserialize_with = "super::number::integer")]
    pub extractor_version: u8,
    /// Whether an estimator was actually observed; never reconstructed from totals.
    pub estimation: Estimation,
    /// Required null unless the existing stream estimator was actually used.
    #[serde(deserialize_with = "nullable")]
    pub estimate: Option<Estimate>,
    /// Exact owned parent execution, never inferred from correlation IDs.
    #[serde(deserialize_with = "nullable")]
    pub parent_execution_id: Option<ExecutionId>,
    /// Independently admitted owned children. Their usage must not be added to these frames.
    pub child_execution_ids: Vec<ExecutionId>,
    /// Attempts in receipt dispatch order, bounded by accepted dispatch.
    pub attempts: Vec<AttemptEvidence>,
    /// Observations in execution order, with bounded prefix retention.
    pub frames: Vec<Frame>,
    /// Sticky once any frame cannot be retained.
    pub truncated: bool,
    /// Saturating exact count of discarded observations.
    #[serde(deserialize_with = "super::number::integer")]
    pub dropped_frames: u64,
}
impl Validate for ExecutionUsageEvidence {
    fn validate(&self) -> Result<(), ValidationError> {
        self.validate_fields()?;
        bytes(self, self.budget()?.max_bytes)
    }
}

impl ExecutionUsageEvidence {
    /// Encode the validated document as compact JSON within its wire byte limit.
    ///
    /// # Errors
    /// Returns the same contract violations as [`Validate::validate`].
    pub fn to_validated_json(&self) -> Result<String, ValidationError> {
        self.validate_fields()?;
        let mut json = Vec::new();
        write_json(self, self.budget()?.max_bytes, &mut json)?;
        String::from_utf8(json).map_err(|_| ValidationError("invalid JSON encoding"))
    }

    fn validate_fields(&self) -> Result<(), ValidationError> {
        self.dispatch.validate_for(&self.tenant_id)?;
        let budget = self.budget()?;
        require(
            self.extractor_version == Self::extractor_version_for(&self.dispatch, &self.frames),
            "usage extractor version",
        )?;
        unique(
            &self
                .attempts
                .iter()
                .map(|a| &a.attempt_id)
                .collect::<Vec<_>>(),
            0,
            self.dispatch.accepted_count(),
        )?;
        require(
            self.frames.len() <= budget.max_frames && self.dropped_frames <= AuthorityRevision::MAX,
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
        match &self.dispatch {
            Dispatch::Strict => require(
                self.frames.iter().all(|frame| {
                    frame.protocol != Protocol::GenerateContent
                        && frame.source != Source::GenerateContentChunk
                }),
                "strict usage evidence protocol",
            )?,
            Dispatch::Ordered { .. } => {
                require(
                    self.parent_execution_id.is_none() && self.child_execution_ids.is_empty(),
                    "ordered evidence linkage",
                )?;
                require(
                    self.frames
                        .iter()
                        .all(|frame| frame.protocol != Protocol::Decisions),
                    "ordered evidence protocol",
                )?;
                require(
                    self.frames
                        .iter()
                        .filter(|frame| frame.protocol == Protocol::GenerateContent)
                        .all(|frame| {
                            frame.quantities.iter().all(|quantity| {
                                quantity.value.is_none_or(|value| value <= i32::MAX as u64)
                            })
                        }),
                    "native quantity range",
                )?;
            }
        }
        validate_observations(&self.attempts, &self.frames)?;
        bytes(&self.frames, budget.max_frame_bytes)
    }

    /// Reservation determined by the accepted selection, never the actual sends.
    ///
    /// # Errors
    /// Rejects an empty or oversized ordered selection.
    pub fn budget(&self) -> Result<EvidenceBudget, ValidationError> {
        EvidenceBudget::for_attempts(self.dispatch.accepted_count())
    }

    /// Select the mapping discriminator from admitted ingress and observed protocols.
    pub fn extractor_version_for(dispatch: &Dispatch, frames: &[Frame]) -> u8 {
        if matches!(dispatch, Dispatch::Ordered { accepted_selection, .. } if accepted_selection.inbound_protocol == super::ordered::GenerationProtocol::GenerateContent)
            || frames
                .iter()
                .any(|frame| frame.protocol == Protocol::GenerateContent)
        {
            2
        } else {
            1
        }
    }

    /// Validate identities, dispatch and all captured actual attempts against the receipt.
    ///
    /// # Errors
    /// Rejects invalid documents, mismatched selection, identities, target or outcome,
    /// and missing, reordered or fabricated captured attempts.
    pub fn validate_against(&self, receipt: &ExecutionReceipt) -> Result<(), ValidationError> {
        self.validate()?;
        receipt.validate()?;
        require(
            self.installation_id == receipt.installation_id
                && self.tenant_id == receipt.tenant_id
                && self.execution_id == receipt.execution_id
                && self.dispatch == receipt.dispatch,
            "usage evidence receipt binding",
        )?;
        if self.capture == Capture::Captured {
            require(
                self.attempts.len() == receipt.attempts.len(),
                "usage evidence actual attempts",
            )?;
            for (evidence, attempt) in self.attempts.iter().zip(&receipt.attempts) {
                require(
                    evidence.attempt_id == attempt.attempt_id
                        && evidence.outcome == attempt.outcome
                        && same_target(evidence.target.as_ref(), attempt.target.as_ref()),
                    "usage evidence attempt binding",
                )?;
            }
        }
        Ok(())
    }
}

// Shared extractor facts; dispatch-specific bounds are checked above.
pub(super) fn validate_observations(
    attempts: &[AttemptEvidence],
    frames: &[Frame],
) -> Result<(), ValidationError> {
    for attempt in attempts {
        if let Some(target) = &attempt.target {
            target.validate()?;
        }
        require(
            !attempt.terminal_observed
                || matches!(attempt.outcome, Outcome::Completed | Outcome::Failed),
            "usage terminal outcome",
        )?;
        require(
            frames
                .iter()
                .filter(|f| f.attempt_id == attempt.attempt_id)
                .count()
                <= MAX_ATTEMPT_FRAMES,
            "attempt frame bound",
        )?;
    }
    for (index, frame) in frames.iter().enumerate() {
        require(
            frame.sequence == index as u64
                && attempts.iter().any(|a| a.attempt_id == frame.attempt_id),
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
                | (Protocol::GenerateContent, Source::GenerateContentChunk)
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

    Ok(())
}

/// Discovery of bounded usage observation capture.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageEvidenceCapabilities {
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
impl UsageEvidenceCapabilities {
    /// Describe the bounded extractor and its absolute dispatch limits.
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
impl Validate for UsageEvidenceCapabilities {
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

fn same_target(left: Option<&Target>, right: Option<&Target>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.provider_id == right.provider_id
                && left.model_id == right.model_id
                && left.account_id == right.account_id
                && left.credential_id == right.credential_id
                && left.credential_version == right.credential_version
        }
        _ => false,
    }
}
