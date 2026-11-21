//! Bounded technical execution facts, without pricing or settlement policy.

use serde::{Deserialize, Serialize};

use super::identity::{
    AccountId, AttemptId, AuthorityRevision, CorrelationId, CredentialId, ExecutionId,
    InstallationId, PrincipalId, ProviderId, RevisionId, TenantId, Timestamp,
};
use super::ordered::{
    AcceptedSelection, MAX_CANDIDATES, validate_attempt_limit, validate_tenant_reference,
};
use super::validation::{Validate, ValidationError, bytes, nullable, require, text, unique};

mod dispatch;

/// Maximum serialized receipt size.
pub const MAX_BYTES: usize = 1024 * 1024;

/// Dispatch facts accepted before any physical provider attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Dispatch {
    /// Strict dispatch admits at most eight physical attempts.
    Strict,
    /// Ordered dispatch retains the exact selection and admission ceiling.
    Ordered {
        /// Accepted route, ingress protocol and original candidate order.
        accepted_selection: AcceptedSelection,
        /// Configured ceiling at admission, at least the accepted count.
        attempt_limit: u8,
    },
}
impl Dispatch {
    /// Maximum number of actual attempts admitted by this dispatch.
    pub fn accepted_count(&self) -> usize {
        match self {
            Self::Strict => 8,
            Self::Ordered {
                accepted_selection, ..
            } => accepted_selection.candidate_indices.len(),
        }
    }
    /// Validate the dispatch selection and its owning tenant.
    ///
    /// # Errors
    /// Rejects invalid selections, limits and foreign tenant references.
    pub fn validate_for(&self, tenant: &TenantId) -> Result<(), ValidationError> {
        if let Self::Ordered {
            accepted_selection,
            attempt_limit,
        } = self
        {
            accepted_selection.validate()?;
            validate_tenant_reference(&accepted_selection.route, tenant)?;
            validate_attempt_limit(*attempt_limit)?;
            require(
                accepted_selection.candidate_indices.len() <= usize::from(*attempt_limit),
                "ordered attempt capacity",
            )?;
        }
        Ok(())
    }
}

/// Evidence provenance, kept conservative across every provider attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Quantities reported by the actual provider.
    Provider,
    /// Estimated quantities; these never constitute complete evidence.
    Estimated,
    /// Unobserved quantities; all counters must be null.
    Unknown,
}

/// Token evidence; missing dimensions remain null, never invented zeroes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    /// Observed input tokens or explicit null.
    #[serde(deserialize_with = "super::number::nullable_integer")]
    pub input_tokens: Option<u64>,
    /// Observed output tokens or explicit null.
    #[serde(deserialize_with = "super::number::nullable_integer")]
    pub output_tokens: Option<u64>,
    /// Observed cache-read tokens or explicit null.
    #[serde(deserialize_with = "super::number::nullable_integer")]
    pub cache_read_tokens: Option<u64>,
    /// Observed cache-write tokens or explicit null.
    #[serde(deserialize_with = "super::number::nullable_integer")]
    pub cache_write_tokens: Option<u64>,
    /// Observed reasoning tokens or explicit null.
    #[serde(deserialize_with = "super::number::nullable_integer")]
    pub reasoning_tokens: Option<u64>,
    /// Source quality of these quantities.
    pub provenance: Provenance,
    /// True only when every supported dimension has provider evidence.
    pub complete: bool,
}
impl Usage {
    fn quantities(&self) -> [Option<u64>; 5] {
        [
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
            self.reasoning_tokens,
        ]
    }
}
impl Validate for Usage {
    fn validate(&self) -> Result<(), ValidationError> {
        let quantities = self.quantities();
        require(
            quantities
                .iter()
                .flatten()
                .all(|&v| v <= AuthorityRevision::MAX),
            "usage integer range",
        )?;
        if self.provenance == Provenance::Unknown {
            require(
                !self.complete && quantities.iter().all(Option::is_none),
                "unknown usage evidence",
            )?;
        }
        if self.complete {
            require(
                self.provenance == Provenance::Provider && quantities.iter().all(Option::is_some),
                "complete usage evidence",
            )?;
        }
        Ok(())
    }
}

/// Actual selected provider/model/account/credential version, never a planned fallback.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// Actual provider identity.
    pub provider_id: ProviderId,
    /// Actual native model identifier, at most 256 characters.
    pub model_id: String,
    /// Nonsecret account identity.
    pub account_id: AccountId,
    /// Nonsecret upstream credential identity.
    pub credential_id: CredentialId,
    /// Exact upstream credential version selected for this attempt.
    pub credential_version: RevisionId,
}
redacted_contract_debug!(Target);

/// Whitelisted native token counters. Arbitrary provider metadata is not retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDimension {
    /// Input tokens.
    InputTokens,
    /// Output tokens.
    OutputTokens,
    /// Cache-read tokens.
    CacheReadTokens,
    /// Cache-write tokens.
    CacheWriteTokens,
    /// Reasoning tokens.
    ReasoningTokens,
}

/// A single bounded native counter; each dimension appears at most once per attempt.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumericUsage {
    /// Whitelisted counter identity.
    pub name: UsageDimension,
    /// Nonnegative, exactly representable JSON integer.
    #[serde(deserialize_with = "super::number::integer")]
    pub value: u64,
}

/// Observed invocation or attempt result, independent of evidence completeness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Observed successful provider completion.
    Completed,
    /// Observed failure.
    Failed,
    /// Cancellation was observed.
    Cancelled,
    /// Insufficient evidence; never automatically redispatch.
    Unknown,
}

/// One provider attempt, including failed attempts that may have consumed tokens.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    /// Unique identity within the execution.
    pub attempt_id: AttemptId,
    /// Accepted candidate ordinal for ordered dispatch; null for strict dispatch.
    #[serde(deserialize_with = "optional_u16")]
    pub candidate_index: Option<u16>,
    /// Observed final upstream HTTP status, or explicit null if unknown.
    #[serde(deserialize_with = "optional_u16")]
    pub observed_http_status: Option<u16>,
    /// Actual selected target; required for completed attempts.
    #[serde(deserialize_with = "nullable")]
    pub target: Option<Target>,
    /// Observed attempt outcome.
    pub outcome: Outcome,
    /// Normalized conservative usage evidence.
    pub usage: Usage,
    /// Unique numeric native dimensions, bounded by count and encoded bytes.
    pub native_usage: Vec<NumericUsage>,
}

/// Durability/classification state of an execution receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptState {
    /// Durable terminal facts have been committed.
    Committed,
    /// Retained facts are incomplete.
    Incomplete,
    /// Recovery cannot establish a complete outcome.
    Unknown,
}

/// Owner-bound facts of one actual invocation. Correlation never deduplicates it.
#[derive(Clone, Debug, Serialize)]
pub struct ExecutionReceipt {
    /// Strict or ordered dispatch, including the accepted ordered selection.
    pub dispatch: Dispatch,
    /// Store that admitted this invocation.
    pub installation_id: InstallationId,
    /// Immutable owning tenant.
    pub tenant_id: TenantId,
    /// Stable authenticated actor, independent of subsequent key rotation.
    pub principal_id: PrincipalId,
    /// Unique actual invocation identity.
    pub execution_id: ExecutionId,
    /// Caller correlation; reused values may match many executions.
    pub correlation_id: CorrelationId,
    /// Approved immutable routing/policy envelope.
    pub execution_revision: RevisionId,
    /// Durability/classification state.
    pub state: ReceiptState,
    /// Observed overall outcome.
    pub outcome: Outcome,
    /// Actual attempts in dispatch order, including failures.
    pub attempts: Vec<Attempt>,
    /// Conservative totals across every attempt.
    pub usage: Usage,
    /// UTC recording timestamp.
    pub recorded_at: Timestamp,
}

#[derive(Deserialize)]
#[serde(remote = "ExecutionReceipt", deny_unknown_fields)]
struct ReceiptFields {
    dispatch: Dispatch,
    installation_id: InstallationId,
    tenant_id: TenantId,
    principal_id: PrincipalId,
    execution_id: ExecutionId,
    correlation_id: CorrelationId,
    execution_revision: RevisionId,
    state: ReceiptState,
    outcome: Outcome,
    attempts: Vec<Attempt>,
    usage: Usage,
    recorded_at: Timestamp,
}

impl<'de> Deserialize<'de> for ExecutionReceipt {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Object;
        impl<'de> serde::de::Visitor<'de> for Object {
            type Value = ExecutionReceipt;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an execution receipt JSON object")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                map: M,
            ) -> Result<Self::Value, M::Error> {
                // Forward original field tokens to the closed derived decoder.
                ReceiptFields::deserialize(serde::de::value::MapAccessDeserializer::new(map))
            }
        }
        deserializer.deserialize_map(Object)
    }
}

/// One bounded, ASCII-ordered correlation query page.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPage {
    /// At most 100 unique invocation identities, strictly ascending.
    pub execution_ids: Vec<ExecutionId>,
    /// Query-wide multiplicity, even when this page has only one item.
    pub multiple_executions: bool,
    /// Last ID when another page exists, otherwise explicit null.
    #[serde(deserialize_with = "nullable")]
    pub next_after: Option<ExecutionId>,
    /// Store whose executions were queried.
    pub installation_id: InstallationId,
    /// Original query correlation.
    pub correlation_id: CorrelationId,
}

impl Validate for Target {
    fn validate(&self) -> Result<(), ValidationError> {
        text(&self.model_id, 256)
    }
}
impl Validate for NumericUsage {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.value <= AuthorityRevision::MAX, "native usage range")
    }
}
impl Validate for Attempt {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.candidate_index
                .is_none_or(|index| usize::from(index) < MAX_CANDIDATES),
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

pub(super) fn validate_attempt_facts(
    outcome: Outcome,
    target: Option<&Target>,
    usage: &Usage,
    native_usage: &[NumericUsage],
) -> Result<(), ValidationError> {
    require(
        outcome != Outcome::Completed || target.is_some(),
        "completed attempt target",
    )?;
    if let Some(target) = target {
        target.validate()?;
    }
    usage.validate()?;
    require(native_usage.len() <= 32, "native usage count")?;
    let mut names = std::collections::HashSet::new();
    for counter in native_usage {
        require(
            names.insert(counter.name),
            "duplicate native usage dimension",
        )?;
        counter.validate()?;
    }
    bytes(&native_usage, 8192)
}

fn optional_u16<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u16>, D::Error> {
    super::number::nullable_integer(deserializer)?
        .map(|value| u16::try_from(value).map_err(serde::de::Error::custom))
        .transpose()
}

fn validate_receipt_facts(
    outcome: Outcome,
    attempts: &[Attempt],
    usage: &Usage,
) -> Result<(), ValidationError> {
    require(
        outcome != Outcome::Completed || attempts.iter().any(|a| a.outcome == Outcome::Completed),
        "completed execution attempt",
    )?;
    let mut ids = std::collections::HashSet::new();
    for attempt in attempts {
        require(
            ids.insert(&attempt.attempt_id),
            "duplicate attempt identity",
        )?;
        attempt.validate()?;
    }
    usage.validate()?;
    let provenance = if attempts.is_empty()
        || attempts
            .iter()
            .any(|a| a.usage.provenance == Provenance::Unknown)
    {
        Provenance::Unknown
    } else if attempts
        .iter()
        .any(|a| a.usage.provenance == Provenance::Estimated)
    {
        Provenance::Estimated
    } else {
        Provenance::Provider
    };
    require(usage.provenance == provenance, "aggregate provenance")?;
    require(
        !usage.complete || (!attempts.is_empty() && attempts.iter().all(|a| a.usage.complete)),
        "aggregate completeness",
    )?;
    for (index, total) in usage.quantities().into_iter().enumerate() {
        if let Some(total) = total {
            require(!attempts.is_empty(), "total without attempts")?;
            let sum = attempts.iter().try_fold(0_u64, |sum, a| {
                sum.checked_add(a.usage.quantities()[index]?)
            });
            require(sum == Some(total), "total must include every attempt")?;
        }
    }

    Ok(())
}

impl Validate for ExecutionReceipt {
    fn validate(&self) -> Result<(), ValidationError> {
        self.dispatch.validate_for(&self.tenant_id)?;
        require(
            self.attempts.len() <= self.dispatch.accepted_count(),
            "attempt count",
        )?;
        match &self.dispatch {
            Dispatch::Strict => require(
                self.attempts
                    .iter()
                    .all(|attempt| attempt.candidate_index.is_none()),
                "strict attempt candidate ordinal",
            )?,
            Dispatch::Ordered {
                accepted_selection, ..
            } => {
                let mut remaining = accepted_selection.candidate_indices.iter();
                for attempt in &self.attempts {
                    require(
                        remaining.any(|&ordinal| Some(ordinal) == attempt.candidate_index),
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
            }
        }
        validate_receipt_facts(self.outcome, &self.attempts, &self.usage)?;
        bytes(self, MAX_BYTES)
    }
}
impl Validate for ExecutionPage {
    fn validate(&self) -> Result<(), ValidationError> {
        unique(&self.execution_ids, 0, 100)?;
        require(
            self.execution_ids.windows(2).all(|pair| pair[0] < pair[1]),
            "execution order",
        )?;
        require(
            self.execution_ids.len() < 2 || self.multiple_executions,
            "execution multiplicity",
        )?;
        require(
            self.next_after
                .as_ref()
                .is_none_or(|after| self.execution_ids.last() == Some(after)),
            "execution cursor",
        )
    }
}
