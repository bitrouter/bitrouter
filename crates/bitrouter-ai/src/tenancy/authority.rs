//! Tenant authority commands and resource grant vocabulary.

use serde::{Deserialize, Serialize};

use super::identity::{
    AuthorityRevision, BindingId, CredentialId, InstallationId, OperationId, OwnerId, PrincipalId,
    ResourceId, RevisionId, TenantId, Timestamp,
};
use super::validation::{Validate, ValidationError, nullable, require, text, unique};

/// Owner namespace; identical IDs in different namespaces are distinct owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerKind {
    /// A tenant owns the resource.
    Tenant,
    /// The installation operator owns an explicitly shared pool resource.
    OperatorPool,
}

/// Resource classes that can appear in an explicit grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    /// A model routing resource.
    Route,
    /// A scheduling or learning policy.
    Policy,
    /// An upstream credential slot.
    Credential,
    /// A learned policy checkpoint.
    Checkpoint,
    /// A framework configuration.
    Configuration,
    /// An execution record.
    Execution,
}

/// Fully qualified resource identity. Neither names nor hashes imply ownership.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceKey {
    /// Ownership namespace.
    pub owner_kind: OwnerKind,
    /// Owner within that namespace.
    pub owner_id: OwnerId,
    /// Resource class.
    pub resource_kind: ResourceKind,
    /// Identity within the owner and class.
    pub resource_id: ResourceId,
}

/// Explicit scope; an empty resource list is invalid, never a wildcard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GrantScope {
    /// All resources of this tenant, plus only explicitly named operator pools.
    Tenant {
        /// No implicit pool access; every key must be operator-owned.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        operator_pools: Vec<ResourceKey>,
    },
    /// Only these fully qualified resource identities.
    Resources {
        /// Nonempty, unique allowlist of at most 256 keys.
        resources: Vec<ResourceKey>,
    },
}
impl Validate for GrantScope {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Tenant { operator_pools } => {
                unique(operator_pools, 0, 256)?;
                require(
                    operator_pools
                        .iter()
                        .all(|key| key.owner_kind == OwnerKind::OperatorPool),
                    "pool owner kind",
                )
            }
            Self::Resources { resources } => unique(resources, 1, 256),
        }
    }
}

/// Fixed tenant roles; installation-operator authority is not a tenant role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantRole {
    /// Read owned framework resources and receipts.
    Viewer,
    /// Draft and execute; does not imply tenant-operator permissions.
    Editor,
    /// Activate policy and manage credentials; does not imply execution permission.
    TenantOperator,
    /// Union of tenant roles, plus binding and issuance management.
    TenantAdmin,
}

/// Fixed delegable tenant permissions; receipt audit grants are provisioned separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Permission {
    /// Read framework resources.
    #[serde(rename = "resource.read")]
    ResourceRead,
    /// Prepare resource drafts.
    #[serde(rename = "resource.draft")]
    ResourceDraft,
    /// Submit execution.
    #[serde(rename = "execution.submit")]
    ExecutionSubmit,
    /// Submit evaluation.
    #[serde(rename = "evaluation.submit")]
    EvaluationSubmit,
    /// Activate or roll back a configuration.
    #[serde(rename = "policy.activate")]
    PolicyActivate,
    /// Use an authorized upstream credential.
    #[serde(rename = "credential.use")]
    CredentialUse,
    /// Inspect/revoke credentials within the current grant ceiling.
    #[serde(rename = "credential.manage")]
    CredentialManage,
    /// Manage bindings within the current grant ceiling.
    #[serde(rename = "binding.manage")]
    BindingManage,
    /// Issue credentials within the current grant ceiling.
    #[serde(rename = "credential.issue")]
    CredentialIssue,
    /// Read tenant execution receipts while the tenant is active.
    #[serde(rename = "receipt.read")]
    ReceiptRead,
}

impl TenantRole {
    /// Return the fixed permission vocabulary for this role; scope and expiry still apply.
    pub const fn permissions(self) -> &'static [Permission] {
        use Permission::*;
        match self {
            Self::Viewer => &[ResourceRead, ReceiptRead],
            Self::Editor => &[
                ResourceRead,
                ReceiptRead,
                ResourceDraft,
                ExecutionSubmit,
                CredentialUse,
                EvaluationSubmit,
            ],
            Self::TenantOperator => &[ResourceRead, ReceiptRead, PolicyActivate, CredentialManage],
            Self::TenantAdmin => &[
                ResourceRead,
                ReceiptRead,
                ResourceDraft,
                ExecutionSubmit,
                CredentialUse,
                EvaluationSubmit,
                PolicyActivate,
                CredentialManage,
                BindingManage,
                CredentialIssue,
            ],
        }
    }
}

/// Public tenant lifecycle; final tombstones are an internal recovery concern.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantState {
    /// New work may be admitted when all other authority checks pass.
    Active,
    /// New work, wakeups and mutations are fenced.
    Suspended,
    /// Draining/classifying retained work before internal tombstoning.
    Deleting,
}

/// Installation-operator command to create a tenant or change its lifecycle by CAS.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantPut {
    /// Stable command identity for lost-response recovery.
    pub operation_id: OperationId,
    /// Null creates; an observed revision updates. Omission is invalid.
    #[serde(deserialize_with = "nullable")]
    pub expected_revision: Option<RevisionId>,
    /// Requested lifecycle.
    pub state: TenantState,
    /// Store identity to which this command was addressed.
    pub installation_id: InstallationId,
}

/// Installation-operator command for a stable, immutable issuer/subject identity.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrincipalPut {
    /// Stable command identity.
    pub operation_id: OperationId,
    /// Null creates; existing principal identity cannot be rebound on update.
    #[serde(deserialize_with = "nullable")]
    pub expected_revision: Option<RevisionId>,
    /// External identity namespace; maximum 256 characters.
    pub issuer: String,
    /// Stable subject within the issuer; maximum 256 characters.
    pub subject: String,
    /// Addressed store identity.
    pub installation_id: InstallationId,
}
redacted_contract_debug!(PrincipalPut);

/// Create/update a single managed principal-to-tenant binding.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingPut {
    /// Stable command identity.
    pub operation_id: OperationId,
    /// Null creates; otherwise CAS against the observed binding revision.
    #[serde(deserialize_with = "nullable")]
    pub expected_revision: Option<RevisionId>,
    /// Recipient principal, independent of the issuing credential.
    pub principal_id: PrincipalId,
    /// Fixed role; grants still intersect with credential permissions.
    pub role: TenantRole,
    /// Explicit tenant or resource scope.
    pub scope: GrantScope,
    /// Upper expiry ceiling for credentials attached to this binding.
    pub expires_at: Timestamp,
    /// CAS managed writer identity; must be null on creation.
    #[serde(deserialize_with = "nullable")]
    pub expected_management_owner: Option<PrincipalId>,
    /// Addressed store identity.
    pub installation_id: InstallationId,
}

/// Issue a credential against exactly one recipient binding, optionally rotating its predecessor.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialCreate {
    /// Stable command identity; replay never returns the secret again.
    pub operation_id: OperationId,
    /// Observed authority snapshot.
    pub expected_authority_revision: AuthorityRevision,
    /// Recipient principal, which must match the binding.
    pub principal_id: PrincipalId,
    /// A nonempty, unique subset of current delegable permissions.
    pub permissions: Vec<Permission>,
    /// Explicit scope bounded by both issuer and recipient binding.
    pub scope: GrantScope,
    /// Expiry bounded by both issuer and recipient binding.
    pub expires_at: Timestamp,
    /// Optional predecessor to revoke in the same authority publication.
    #[serde(deserialize_with = "nullable")]
    pub rotate_credential_id: Option<CredentialId>,
    /// The single recipient binding; disjoint grants cannot be combined.
    pub binding_id: BindingId,
    /// Addressed store identity.
    pub installation_id: InstallationId,
}

/// One-time credential issuance response; diagnostics deliberately omit its secret.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialIssued {
    /// Original operation identity.
    pub operation_id: OperationId,
    /// Nonsecret credential identity retained by replay/lookup.
    pub credential_id: CredentialId,
    /// Stable recipient identity.
    pub principal_id: PrincipalId,
    /// Bound ownership domain.
    pub tenant_id: TenantId,
    /// Authority revision after publication.
    pub authority_revision: AuthorityRevision,
    /// Present only for the first successful issuance response; never recoverable.
    #[serde(deserialize_with = "nullable")]
    pub secret: Option<String>,
    /// Credential expiry.
    pub expires_at: Timestamp,
    /// CAS revision for safe recovery/revocation after a lost response.
    pub credential_revision: RevisionId,
}
redacted_contract_debug!(CredentialIssued);

/// Installation-operator issuance of a receipt-only audit reader.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditReaderCreate {
    /// Stable command identity.
    pub operation_id: OperationId,
    /// Stable reader identity.
    pub principal_id: PrincipalId,
    /// Observed authority snapshot.
    pub expected_authority_revision: AuthorityRevision,
    /// Audit access expiry.
    pub expires_at: Timestamp,
    /// Addressed store identity.
    pub installation_id: InstallationId,
}

/// Observed ingress credential lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialState {
    /// Eligible only while its binding, tenant and permissions also authorize the operation.
    Active,
    /// Explicitly revoked.
    Revoked,
    /// Its expiry is in the past.
    Expired,
}

/// Nonsecret status used to recover a stale CAS revision.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialStatus {
    /// Bound ownership domain.
    pub tenant_id: TenantId,
    /// Nonsecret credential identity.
    pub credential_id: CredentialId,
    /// Current credential CAS revision.
    pub credential_revision: RevisionId,
    /// Current authority snapshot.
    pub authority_revision: AuthorityRevision,
    /// Current credential lifecycle.
    pub state: CredentialState,
}

impl Validate for TenantPut {
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}
impl Validate for PrincipalPut {
    fn validate(&self) -> Result<(), ValidationError> {
        text(&self.issuer, 256)?;
        text(&self.subject, 256)
    }
}
impl Validate for BindingPut {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.expected_revision.is_some() || self.expected_management_owner.is_none(),
            "binding creation owner",
        )?;
        self.scope.validate()
    }
}
impl Validate for CredentialCreate {
    fn validate(&self) -> Result<(), ValidationError> {
        unique(&self.permissions, 1, 10)?;
        self.scope.validate()
    }
}
impl Validate for CredentialIssued {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(secret) = &self.secret {
            text(secret, 512)?;
        }
        Ok(())
    }
}
impl Validate for AuditReaderCreate {
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}
impl Validate for CredentialStatus {
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}
