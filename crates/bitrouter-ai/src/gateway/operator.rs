//! Explicit operator commands and secret-free receipts for a shared gateway.
//!
//! Secret-bearing write inputs are separate from read projections. No type here
//! performs authentication, discovers credentials, retries a mutation or does I/O.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::execution::{GatewayExecution, ResolveAttemptRequest};

/// Capabilities granted by a separate operator identity, never an inference key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    Read,
    PolicyWrite,
    CredentialWrite,
    Publish,
    RotateCorrelation,
    ResolveExecution,
    /// Existing virtual principals are database-wide, not namespace-scoped.
    GlobalPrincipalWrite,
}

/// The two independently advancing components of shared routing authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub config_generation: u64,
    pub key_generation: u64,
}

/// One caller-chosen identity is retained across uncertain delivery and lookup.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    pub operation_id: String,
    pub command: Command,
}

/// Short administrative mutations; no command dispatches provider inference.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(
    tag = "action",
    content = "input",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Command {
    PutTenant(TenantPut),
    PutResource(ResourcePut),
    PutPrincipal(PrincipalPut),
    ImportCredential(CredentialImport),
    Publish(PublicationPut),
    RotateCorrelation {
        expected_generation: u64,
    },
    ResolveExecution {
        execution_id: String,
        attempt_id: String,
        resolution: ResolveAttemptRequest,
    },
}

/// Tenant policy changes preserve existing occupancy and rate accounting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TenantLimits {
    pub concurrent_executions: u32,
    pub requests_per_minute: u32,
    pub resource_pools: BTreeSet<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TenantPut {
    pub tenant_id: String,
    /// None means create-only. This is a policy revision, not occupancy revision.
    pub expected_revision: Option<u64>,
    pub limits: TenantLimits,
}

#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResourcePut {
    pub resource_pool_id: String,
    pub expected_revision: Option<u64>,
    pub concurrent_attempts: u32,
}

/// Explicit database-wide principal administration. Plaintext keys are not sent.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PrincipalPut {
    pub principal_id: String,
    pub tenant_id: String,
    /// Opaque authorization revision returned by a principal read; None creates.
    pub expected_authorization_revision: Option<String>,
    /// SHA-256 verifier of a client-generated high-entropy secret; required on
    /// create, optional on update. Omission preserves the existing verifier.
    pub key_hash: Option<String>,
    pub active: bool,
    pub expires_at_ms: Option<i64>,
    pub requests_per_minute: Option<u32>,
    pub policy_id: Option<String>,
}

/// Write-only provider material; responses expose references and versions only.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialInput {
    ApiKey {
        value: String,
    },
    Oauth {
        access_token: String,
        refresh_token: Option<String>,
        /// Unix seconds; zero denotes a non-expiring credential.
        expires_at: u64,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CredentialImport {
    pub provider: String,
    pub account: String,
    pub resource_pool_id: String,
    pub expected_version: Option<u64>,
    pub credential: CredentialInput,
}

/// One route account references an explicitly imported credential slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CredentialBinding {
    pub provider: String,
    pub account: String,
    pub credential_provider: String,
    pub credential_account: String,
}

/// Complete write-only configuration. Embedded documents are interpreted by the
/// service; clients neither expand environment references nor convert protocols.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Publication {
    pub format: u32,
    pub routing: serde_json::Value,
    #[serde(default)]
    pub policies: Vec<serde_json::Value>,
    pub policy_lock: Option<serde_json::Value>,
    pub credentials: Vec<CredentialBinding>,
    #[serde(default)]
    pub require_known_pricing: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PublicationPut {
    /// CAS checks config_generation; correlation-only rotation may coexist.
    pub expected_generation: u64,
    pub configuration: Publication,
}

/// A bounded, create-only seed for explicit local bootstrap. The service commits
/// the complete plan atomically; ordinary replicas never consume this input.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct BootstrapPlan {
    #[serde(default)]
    pub resources: Vec<ResourcePut>,
    #[serde(default)]
    pub tenants: Vec<TenantPut>,
    #[serde(default)]
    pub principals: Vec<PrincipalPut>,
    #[serde(default)]
    pub credentials: Vec<CredentialImport>,
    pub configuration: Publication,
}

/// The original successful bootstrap, including the first publication receipt.
/// Replaying it does not overwrite configuration that was subsequently changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct BootstrapReceipt {
    pub namespace: String,
    pub operator_id: String,
    pub operation_id: String,
    pub committed_at_ms: i64,
    pub revision: Revision,
    pub operations: Vec<Receipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TenantPolicy {
    pub tenant_id: String,
    pub revision: u64,
    pub limits: TenantLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResourcePolicy {
    pub resource_pool_id: String,
    pub revision: u64,
    pub concurrent_attempts: u32,
}

/// Principal status excludes the key verifier as well as the plaintext key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PrincipalMetadata {
    pub principal_id: String,
    pub tenant_id: String,
    pub authorization_revision: String,
    pub active: bool,
    pub expires_at_ms: Option<i64>,
    pub requests_per_minute: Option<u32>,
    pub policy_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CredentialMetadata {
    pub provider: String,
    pub account: String,
    pub resource_pool_id: String,
    pub version: u64,
}

/// Immutable output committed with its mutation; it is not a fresh status read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Outcome {
    Tenant(TenantPolicy),
    Resource(ResourcePolicy),
    Principal(PrincipalMetadata),
    Credential(CredentialMetadata),
    Published(Revision),
    CorrelationRotated(Revision),
    Execution(GatewayExecution),
}

/// Mutation, audit identity and receipt share one authoritative commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub namespace: String,
    pub operator_id: String,
    pub operation_id: String,
    pub committed_at_ms: i64,
    pub outcome: Outcome,
}

/// Receipt absence never proves that an earlier in-flight transaction failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationLookup {
    Committed { receipt: Box<Receipt> },
    Unresolved { operation_id: String },
}

/// Stable error categories; details must never contain backend or input excerpts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    Forbidden,
    InvalidInput,
    Conflict,
    NotFound,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ErrorResponse {
    pub code: ErrorCode,
    pub operation_id: Option<String>,
}

redacted_contract_debug!(
    Mutation,
    Command,
    TenantPut,
    ResourcePut,
    PrincipalPut,
    CredentialInput,
    CredentialImport,
    Publication,
    PublicationPut,
    BootstrapPlan
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_diagnostics_never_export_secret_material() -> Result<(), serde_json::Error> {
        let secret = "fixture-secret-must-not-appear";
        let input = Mutation {
            operation_id: "fixture-operation".into(),
            command: Command::ImportCredential(CredentialImport {
                provider: "fixture".into(),
                account: "default".into(),
                resource_pool_id: "fixture".into(),
                expected_version: None,
                credential: CredentialInput::ApiKey {
                    value: secret.into(),
                },
            }),
        };
        assert!(!format!("{input:?}").contains(secret));
        // Write serialization carries the explicit input, while read DTOs have
        // no credential material field and reject an attempted secret injection.
        assert!(serde_json::to_string(&input)?.contains(secret));
        assert!(
            serde_json::from_value::<CredentialMetadata>(serde_json::json!({
                "provider":"fixture", "account":"default", "resource_pool_id":"fixture",
                "version":1, "credential":secret
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<OperationLookup>(serde_json::json!({
                "status":"failed", "operation_id":"fixture-operation"
            }))
            .is_err()
        );
        Ok(())
    }
}
