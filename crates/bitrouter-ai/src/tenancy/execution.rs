//! Bounded technical execution facts, without pricing or settlement policy.

use serde::{Deserialize, Serialize};

use super::identity::{
    AccountId, AttemptId, AuthorityRevision, CorrelationId, CredentialId, ExecutionId,
    InstallationId, PrincipalId, ProviderId, RevisionId, TenantId, Timestamp,
};
use super::validation::{Validate, ValidationError, bytes, nullable, require, text, unique};

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
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionReceipt {
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
    /// At most eight distinct attempts, including failures.
    pub attempts: Vec<Attempt>,
    /// Conservative totals across every attempt.
    pub usage: Usage,
    /// UTC recording timestamp.
    pub recorded_at: Timestamp,
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
            self.outcome != Outcome::Completed || self.target.is_some(),
            "completed attempt target",
        )?;
        if let Some(target) = &self.target {
            target.validate()?;
        }
        self.usage.validate()?;
        require(self.native_usage.len() <= 32, "native usage count")?;
        let mut names = std::collections::HashSet::new();
        for counter in &self.native_usage {
            require(
                names.insert(counter.name),
                "duplicate native usage dimension",
            )?;
            counter.validate()?;
        }
        bytes(&self.native_usage, 8192)
    }
}
impl Validate for ExecutionReceipt {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.attempts.len() <= 8, "attempt count")?;
        require(
            self.outcome != Outcome::Completed
                || self
                    .attempts
                    .iter()
                    .any(|a| a.outcome == Outcome::Completed),
            "completed execution attempt",
        )?;
        let mut ids = std::collections::HashSet::new();
        for attempt in &self.attempts {
            require(
                ids.insert(&attempt.attempt_id),
                "duplicate attempt identity",
            )?;
            attempt.validate()?;
        }
        self.usage.validate()?;
        let provenance = if self.attempts.is_empty()
            || self
                .attempts
                .iter()
                .any(|a| a.usage.provenance == Provenance::Unknown)
        {
            Provenance::Unknown
        } else if self
            .attempts
            .iter()
            .any(|a| a.usage.provenance == Provenance::Estimated)
        {
            Provenance::Estimated
        } else {
            Provenance::Provider
        };
        require(self.usage.provenance == provenance, "aggregate provenance")?;
        require(
            !self.usage.complete
                || (!self.attempts.is_empty() && self.attempts.iter().all(|a| a.usage.complete)),
            "aggregate completeness",
        )?;
        for (index, total) in self.usage.quantities().into_iter().enumerate() {
            if let Some(total) = total {
                require(!self.attempts.is_empty(), "total without attempts")?;
                let sum = self.attempts.iter().try_fold(0_u64, |sum, a| {
                    sum.checked_add(a.usage.quantities()[index]?)
                });
                require(sum == Some(total), "total must include every attempt")?;
            }
        }
        bytes(self, 1_048_576)
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
