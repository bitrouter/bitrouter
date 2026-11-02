//! Versioned resource provisioning contracts. Validation does not grant authority.
//!
//! Provider secrets are write-only. Route and policy documents are compiled by
//! the service; this crate neither resolves their configuration nor executes it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "schema")]
mod schema;

use super::{
    authority::{ResourceKey, ResourceKind},
    configuration::ResourceRef,
    identity::{AuthorityRevision, InstallationId, OperationId, RevisionId},
    validation::{Validate, ValidationError, bytes, nullable, require, text, unique},
};

/// One explicit provider/account binding to an immutable upstream credential.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CredentialBinding {
    /// Provider name in the route's explicit routing source.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 128)))]
    pub provider: String,
    /// Explicit account label, including `default` for a single account.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 128)))]
    pub account: String,
    /// Authorized credential slot revision; never a secret or an ambient key.
    pub credential: ResourceRef,
}

/// Write-only resource definition. Debug output deliberately omits all contents.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Definition {
    /// Static upstream credential; ingress credentials use the separate issuance API.
    Credential {
        /// Nonempty secret, at most 65,536 UTF-8 bytes; never returned by a read.
        #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 65536)))]
        secret: String,
    },
    /// Explicit routing source, without ambient providers or inline credentials.
    Route {
        /// Service routing document (providers, models, routers and defaults).
        #[cfg_attr(
            feature = "schema",
            schemars(with = "std::collections::BTreeMap<String, Value>")
        )]
        routing: Value,
        /// Selector resolved exclusively inside this document.
        #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 512)))]
        selector: String,
        /// Exact policy revision if the selector uses a policy.
        #[serde(deserialize_with = "nullable")]
        #[cfg_attr(
            feature = "schema",
            schemars(with = "schema::RequiredNullable<ResourceRef>")
        )]
        policy: Option<ResourceRef>,
        /// Separately owned credential slots; at most 256 unique accounts.
        #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 256)))]
        credentials: Vec<CredentialBinding>,
    },
    /// Policy-as-code, interpreted and validated exclusively by the service.
    Policy {
        /// Explicit policy document; filesystem paths are not authority.
        #[cfg_attr(
            feature = "schema",
            schemars(with = "std::collections::BTreeMap<String, Value>")
        )]
        document: Value,
        /// Selected policy name in this document.
        #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 256)))]
        name: String,
        /// Explicit immutable dependencies; at most 256 references.
        #[cfg_attr(feature = "schema", schemars(length(max = 256)))]
        dependencies: Vec<ResourceRef>,
    },
}
redacted_contract_debug!(Definition);

impl Definition {
    /// Resource class required for the addressed key.
    pub fn resource_kind(&self) -> ResourceKind {
        match self {
            Self::Credential { .. } => ResourceKind::Credential,
            Self::Route { .. } => ResourceKind::Route,
            Self::Policy { .. } => ResourceKind::Policy,
        }
    }
}

/// Create or rotate a resource. The server assigns the immutable revision.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResourcePut {
    /// Resource provisioning format, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1)))]
    pub format: u32,
    /// Stable identity for replay and unknown-outcome recovery.
    pub operation_id: OperationId,
    /// Addressed persistent store identity.
    pub installation_id: InstallationId,
    /// Explicit owner and resource identity, independent of the route name.
    pub key: ResourceKey,
    /// Null creates a new managed resource; otherwise compare the managed head.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "schema::RequiredNullable<RevisionId>")
    )]
    pub expected_revision: Option<RevisionId>,
    /// Write-only immutable content; at most 1 MiB including the command.
    pub definition: Definition,
}
redacted_contract_debug!(ResourcePut);

/// Revoke one immutable revision without deleting historical execution evidence.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResourceRevoke {
    /// Resource provisioning format, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1)))]
    pub format: u32,
    /// Stable command identity.
    pub operation_id: OperationId,
    /// Addressed persistent store identity.
    pub installation_id: InstallationId,
    /// Exact target; both current and older revisions may be revoked.
    pub reference: ResourceRef,
    /// Observed authority revision; revocation is not an implicit latest write.
    pub expected_authority_revision: AuthorityRevision,
}

/// Nonsecret status of one retained revision.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResourceStatus {
    /// Resource provisioning format, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1)))]
    pub format: u32,
    /// Addressed persistent store identity.
    pub installation_id: InstallationId,
    /// Exact immutable resource identity.
    pub reference: ResourceRef,
    /// Current authority snapshot, independent of the content revision.
    pub authority_revision: AuthorityRevision,
    /// Most recent remotely managed revision; null for manifest-only resources.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "schema::RequiredNullable<RevisionId>")
    )]
    pub head_revision: Option<RevisionId>,
    /// Revocation fences subsequent dispatch and reuse, including cached routes.
    pub revoked: bool,
}

/// Independently versioned descriptor; does not alter strict v1 discovery.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResourceCapabilities {
    /// Resource provisioning contract, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1)))]
    pub format: u32,
    /// Persistent store identity.
    pub installation_id: InstallationId,
    /// Encrypted provisioning is available with the current wrapping key.
    pub provisioning_enabled: bool,
    /// Maximum serialized command bytes.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1048576)))]
    pub max_command_bytes: u32,
    /// Endpoint selection by tenant credentials requires host-approved origins.
    pub tenant_routes_require_approved_origins: bool,
}

impl Validate for Definition {
    fn validate(&self) -> Result<(), ValidationError> {
        bytes(self, 1024 * 1024)?;
        match self {
            Self::Credential { secret } => require(
                !secret.is_empty() && secret.len() <= 65_536,
                "upstream credential length",
            ),
            Self::Route {
                routing,
                selector,
                policy,
                credentials,
            } => {
                require(routing.is_object(), "routing document")?;
                text(selector, 512)?;
                require(
                    policy
                        .as_ref()
                        .is_none_or(|p| p.resource_kind == ResourceKind::Policy),
                    "policy reference",
                )?;
                require(
                    !credentials.is_empty() && credentials.len() <= 256,
                    "credential bindings",
                )?;
                let mut slots = std::collections::HashSet::new();
                for binding in credentials {
                    text(&binding.provider, 128)?;
                    text(&binding.account, 128)?;
                    require(
                        binding.credential.resource_kind == ResourceKind::Credential,
                        "credential reference",
                    )?;
                    require(
                        slots.insert((&binding.provider, &binding.account)),
                        "duplicate credential binding",
                    )?;
                }
                Ok(())
            }
            Self::Policy {
                document,
                name,
                dependencies,
            } => {
                require(document.is_object(), "policy document")?;
                text(name, 256)?;
                unique(dependencies, 0, 256)?;
                require(
                    dependencies.iter().all(|r| {
                        matches!(
                            r.resource_kind,
                            ResourceKind::Credential | ResourceKind::Checkpoint
                        )
                    }),
                    "policy dependency kind",
                )
            }
        }
    }
}
impl Validate for ResourcePut {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.format == 1, "resource format")?;
        require(
            self.key.resource_kind == self.definition.resource_kind(),
            "resource kind",
        )?;
        self.definition.validate()?;
        bytes(self, 1024 * 1024)
    }
}
impl Validate for ResourceRevoke {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.format == 1, "resource format")
    }
}
impl Validate for ResourceStatus {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.format == 1, "resource format")
    }
}
impl Validate for ResourceCapabilities {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.format == 1
                && self.max_command_bytes > 0
                && self.max_command_bytes <= 1024 * 1024
                && self.tenant_routes_require_approved_origins,
            "resource capabilities",
        )
    }
}
