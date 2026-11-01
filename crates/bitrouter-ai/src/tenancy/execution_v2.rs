//! Ordered execution facts with explicit accepted selection and physical attempts.

use serde::{Deserialize, Serialize};

use super::{
    execution::{
        NumericUsage, Outcome, ReceiptAttempt, ReceiptState, Target, Usage, validate_attempt_facts,
        validate_receipt_facts,
    },
    identity::{
        AttemptId, CorrelationId, ExecutionId, InstallationId, PrincipalId, RevisionId, TenantId,
        Timestamp,
    },
    ordered::{
        AcceptedSelectionV2, MAX_CANDIDATES, validate_attempt_limit, validate_tenant_reference,
    },
    validation::{Validate, ValidationError, bytes, nullable, require},
};

/// Maximum serialized version-two receipt size.
pub const MAX_BYTES: usize = 1024 * 1024;

/// One actual provider attempt, never an unsent or locally refused candidate.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptV2 {
    /// Unique physical attempt identity within the execution.
    pub attempt_id: AttemptId,
    /// Persisted root ordinal within the accepted protocol selection.
    #[serde(deserialize_with = "super::number::integer")]
    pub candidate_index: u16,
    /// Actual target; required for a completed attempt.
    #[serde(deserialize_with = "nullable")]
    pub target: Option<Target>,
    /// Observed terminal result; unknown work must not be inferred successful.
    pub outcome: Outcome,
    /// Observed final upstream HTTP status, 200–599, or explicit null if unknown.
    #[serde(deserialize_with = "observed_status")]
    pub observed_http_status: Option<u16>,
    /// Conservative normalized usage, with absent counters left null.
    pub usage: Usage,
    /// Bounded, unique native usage counters.
    pub native_usage: Vec<NumericUsage>,
}
impl Validate for AttemptV2 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            usize::from(self.candidate_index) < MAX_CANDIDATES,
            "attempt candidate ordinal",
        )?;
        require(
            self.observed_http_status
                .is_none_or(|status| (200..=599).contains(&status)),
            "observed HTTP status",
        )?;
        require(
            self.outcome != Outcome::Completed
                || self
                    .observed_http_status
                    .is_none_or(|status| (200..=299).contains(&status)),
            "completed attempt HTTP status",
        )?;
        validate_attempt_facts(
            self.outcome,
            self.target.as_ref(),
            &self.usage,
            &self.native_usage,
        )
    }
}
impl ReceiptAttempt for AttemptV2 {
    fn id(&self) -> &AttemptId {
        &self.attempt_id
    }
    fn outcome(&self) -> Outcome {
        self.outcome
    }
    fn usage(&self) -> &Usage {
        &self.usage
    }
}

fn observed_status<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u16>, D::Error> {
    super::number::nullable_integer(deserializer)?
        .map(|value| u16::try_from(value).map_err(serde::de::Error::custom))
        .transpose()
}

/// Owner-bound version-two receipt. Version-one readers must refuse this format.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionReceiptV2 {
    /// Receipt format, exactly two.
    #[serde(deserialize_with = "super::number::integer")]
    pub schema_version: u8,
    /// Store that admitted this invocation.
    pub installation_id: InstallationId,
    /// Immutable owning tenant.
    pub tenant_id: TenantId,
    /// Stable authenticated actor.
    pub principal_id: PrincipalId,
    /// Unique actual invocation identity.
    pub execution_id: ExecutionId,
    /// Caller correlation; never an idempotency key.
    pub correlation_id: CorrelationId,
    /// Exact approved immutable routing revision.
    pub execution_revision: RevisionId,
    /// Accepted route, ingress protocol and original candidate order.
    pub accepted_selection: AcceptedSelectionV2,
    /// Configured ceiling at admission, 1–64, at least the accepted count.
    #[serde(deserialize_with = "super::number::integer")]
    pub attempt_limit: u8,
    /// Existing durability/classification semantics, including incomplete/unknown.
    pub state: ReceiptState,
    /// Observed overall outcome, independent of evidence completeness.
    pub outcome: Outcome,
    /// Actual attempts in order; ordinals form a unique subsequence of selection.
    pub attempts: Vec<AttemptV2>,
    /// Conservative usage across every actual attempt, including failures.
    pub usage: Usage,
    /// UTC recording timestamp.
    pub recorded_at: Timestamp,
}
impl Validate for ExecutionReceiptV2 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 2, "ordered receipt version")?;
        self.accepted_selection.validate()?;
        validate_tenant_reference(&self.accepted_selection.route, &self.tenant_id)?;
        validate_attempt_limit(self.attempt_limit)?;
        let selected = &self.accepted_selection.candidate_indices;
        require(
            selected.len() <= usize::from(self.attempt_limit),
            "ordered attempt capacity",
        )?;
        require(
            self.attempts.len() <= selected.len(),
            "ordered actual attempt count",
        )?;
        let mut remaining = selected.iter();
        for attempt in &self.attempts {
            require(
                remaining.any(|&ordinal| ordinal == attempt.candidate_index),
                "ordered attempt subsequence",
            )?;
        }
        require(
            self.attempts
                .windows(2)
                .all(|pair| pair[0].outcome == Outcome::Failed),
            "ordered attempt after terminal outcome",
        )?;
        if self
            .attempts
            .iter()
            .any(|attempt| attempt.outcome == Outcome::Unknown)
        {
            require(
                self.state == ReceiptState::Unknown && self.outcome == Outcome::Unknown,
                "unknown ordered attempt classification",
            )?;
        }
        validate_receipt_facts(self.outcome, &self.attempts, &self.usage)?;
        bytes(self, MAX_BYTES)
    }
}
