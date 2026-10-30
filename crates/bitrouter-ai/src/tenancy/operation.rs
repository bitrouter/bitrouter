//! Management operation outcomes and dispatch capability binding.

use serde::{Deserialize, Serialize};

use super::identity::{
    AuthorityRevision, CorrelationId, ExecutionId, InstallationId, InstanceId, OperationId,
    ResourceId, RevisionId,
};
use super::validation::{Validate, ValidationError, nullable, require, text};

/// CAS command shared by credential revocation and managed-writer detach.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    /// Stable command identity.
    pub operation_id: OperationId,
    /// Required observed entity revision; never null or implicit latest.
    pub expected_revision: RevisionId,
    /// Addressed store identity.
    pub installation_id: InstallationId,
}

/// Stable public error vocabulary without resource or secret disclosure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Invalid or ambiguous input.
    InvalidRequest,
    /// Missing or invalid authentication.
    Unauthorized,
    /// Current authority does not grant this operation.
    Forbidden,
    /// Tenant lifecycle fences this operation.
    TenantInactive,
    /// Owned resource absent or hidden.
    NotFound,
    /// Observed revision differs from the retained revision.
    RevisionConflict,
    /// This operation ID already identifies a different command.
    OperationConflict,
    /// Result aged out; its ID remains permanently reserved.
    OperationExpired,
    /// Input exceeds a byte limit.
    PayloadTooLarge,
    /// Bounded technical capacity is exhausted.
    CapacityExceeded,
    /// Current authority cannot be established.
    AuthorityUnavailable,
    /// Required durable storage is unavailable.
    StorageUnavailable,
    /// Dispatch metadata no longer matches the serving capability.
    CapabilityMismatch,
}

/// Evidence of whether provider work began; network failure never implies not-started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    /// Positive evidence that no provider work began.
    NotStarted,
    /// Provider work began.
    Started,
    /// Evidence is insufficient to decide.
    Unknown,
}

/// Structured tenancy service failure. Default diagnostics omit its free-form message.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Error {
    /// Machine-readable failure class.
    pub code: ErrorCode,
    /// Redacted service explanation, at most 512 characters.
    pub message: String,
    /// Positive dispatch evidence, or unknown.
    pub dispatch_state: DispatchState,
    /// Known management operation, otherwise explicit null.
    #[serde(deserialize_with = "nullable")]
    pub operation_id: Option<OperationId>,
    /// Known execution, otherwise explicit null.
    #[serde(deserialize_with = "nullable")]
    pub execution_id: Option<ExecutionId>,
}
redacted_contract_debug!(Error);

/// Persisted management outcome, distinct from inference idempotency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    /// Admitted management work has not reached a final result.
    Pending,
    /// Mutation and result committed, then authority published.
    Applied,
    /// Command rejected without its requested mutation.
    Rejected,
    /// Outcome requires explicit recovery; do not replay under a new ID.
    Unknown,
}

/// Nonsecret result, readable only with current authority over its saved context.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationResult {
    /// Original command identity.
    pub operation_id: OperationId,
    /// Persisted outcome.
    pub status: OperationStatus,
    /// Published authority snapshot, required for applied results.
    #[serde(deserialize_with = "nullable")]
    pub authority_revision: Option<AuthorityRevision>,
    /// Affected entity revision, required for applied results.
    #[serde(deserialize_with = "nullable")]
    pub resource_revision: Option<RevisionId>,
    /// Nonsecret affected identity, required for applied results.
    #[serde(deserialize_with = "nullable")]
    pub resource_id: Option<ResourceId>,
    /// Required on rejection, null on success.
    #[serde(deserialize_with = "nullable")]
    pub error: Option<Error>,
}

/// Fresh service capabilities; discovery alone does not authorize execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantCapabilities {
    /// Supported tenancy contract, currently one.
    #[serde(deserialize_with = "super::number::integer")]
    pub contract_version: u32,
    /// Stable local authority store identity.
    pub installation_id: InstallationId,
    /// Fresh identity for this process incarnation.
    pub instance_id: InstanceId,
    /// Changes whenever dispatch eligibility or mode changes.
    pub capability_revision: RevisionId,
    /// Exact retained execution revisions are enforced.
    pub revision_bound_dispatch: bool,
    /// Durable strict terminal receipts are available.
    pub strict_receipts: bool,
    /// New dispatch is currently admitted under authority and storage fences.
    pub dispatch_enabled: bool,
}

/// Exact trusted metadata for one strict submission; no automatic refresh/retry is implied.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchBinding {
    /// HTTP tenancy contract version, exactly `1`.
    #[serde(rename = "Bitrouter-Tenancy-Version")]
    pub contract_version: String,
    /// Addressed persistent authority store.
    #[serde(rename = "Bitrouter-Installation-Id")]
    pub installation_id: InstallationId,
    /// Addressed process incarnation.
    #[serde(rename = "Bitrouter-Instance-Id")]
    pub instance_id: InstanceId,
    /// Observed dispatch capability revision.
    #[serde(rename = "Bitrouter-Capability-Revision")]
    pub capability_revision: RevisionId,
    /// Approved immutable execution revision.
    #[serde(rename = "Bitrouter-Execution-Revision")]
    pub execution_revision: RevisionId,
    /// Correlation only; each invocation still creates a distinct execution.
    #[serde(rename = "Bitrouter-Correlation-Id")]
    pub correlation_id: CorrelationId,
    /// Receipt mode, exactly `strict`.
    #[serde(rename = "Bitrouter-Receipt-Mode")]
    pub receipt_mode: String,
}

impl TenantCapabilities {
    /// Bind an explicitly chosen execution revision and correlation to this eligible descriptor.
    ///
    /// # Errors
    /// Rejects unsupported versions or any disabled dispatch guarantee. The server
    /// must recheck this descriptor and current authority when work is admitted.
    pub fn bind(
        &self,
        execution_revision: RevisionId,
        correlation_id: CorrelationId,
    ) -> Result<DispatchBinding, ValidationError> {
        self.validate()?;
        require(
            self.revision_bound_dispatch && self.strict_receipts && self.dispatch_enabled,
            "strict dispatch ineligible",
        )?;
        Ok(DispatchBinding {
            contract_version: "1".into(),
            installation_id: self.installation_id.clone(),
            instance_id: self.instance_id.clone(),
            capability_revision: self.capability_revision.clone(),
            execution_revision,
            correlation_id,
            receipt_mode: "strict".into(),
        })
    }
}

impl Validate for Mutation {
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}
impl Validate for Error {
    fn validate(&self) -> Result<(), ValidationError> {
        text(&self.message, 512)
    }
}
impl Validate for OperationResult {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.status == OperationStatus::Applied {
            require(
                self.authority_revision.is_some()
                    && self.resource_revision.is_some()
                    && self.resource_id.is_some()
                    && self.error.is_none(),
                "applied operation evidence",
            )?;
        }
        if self.status == OperationStatus::Rejected {
            require(self.error.is_some(), "rejected operation error")?;
        }
        if let Some(error) = &self.error {
            error.validate()?;
        }
        Ok(())
    }
}
impl Validate for TenantCapabilities {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.contract_version == 1, "tenancy contract version")
    }
}
impl Validate for DispatchBinding {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.contract_version == "1" && self.receipt_mode == "strict",
            "strict dispatch metadata",
        )
    }
}
