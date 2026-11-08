//! Nonsecret operator directory observations. Recorded grants never confer authority.

use serde::{Deserialize, Serialize};

use super::{
    authority::{CredentialState, GrantScope, Permission, TenantRole, TenantState},
    identity::{
        AuthorityRevision, BindingId, CredentialId, InstallationId, PrincipalId, RevisionId,
        TenantId, Timestamp,
    },
    validation::{Validate, ValidationError, bytes, nullable, require, text, unique},
};

/// Maximum serialized directory page size, including its envelope and cursor.
pub const MAX_DIRECTORY_PAGE_BYTES: usize = 1024 * 1024;

/// Exclusive ASCII ordering key, encoded as `v1|kind|tenant_id|id`.
/// Tenant and principal entries use an empty tenant component. A cursor must
/// always be accompanied by the authority revision of its originating page.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DirectoryCursorV1(String);
redacted_contract_debug!(DirectoryCursorV1);

impl PartialOrd for DirectoryCursorV1 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for DirectoryCursorV1 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for DirectoryCursorV1 {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "DirectoryCursorV1".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string", "maxLength": 271,
            "pattern": "^v1\\|(?:(?:binding|credential)\\|[A-Za-z0-9][A-Za-z0-9._:-]{0,127}|(?:principal|tenant)\\|)\\|[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$"
        })
    }
}

impl DirectoryCursorV1 {
    /// Borrow the validated cursor for a query parameter.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Borrow the `(kind, tenant_id, id)` ordering components.
    pub fn key(&self) -> (&str, &str, &str) {
        let mut parts = self.0.split('|').skip(1);
        (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        )
    }
}
impl TryFrom<String> for DirectoryCursorV1 {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        require(value.len() <= 271, "directory cursor")?;
        let parts: Vec<_> = value.split('|').collect();
        require(parts.len() == 4 && parts[0] == "v1", "directory cursor")?;
        require(
            match parts[1] {
                "binding" | "credential" => TenantId::try_from(parts[2]).is_ok(),
                "principal" | "tenant" => parts[2].is_empty(),
                _ => false,
            } && RevisionId::try_from(parts[3]).is_ok(),
            "directory cursor",
        )?;
        Ok(Self(value))
    }
}
impl TryFrom<&str> for DirectoryCursorV1 {
    type Error = ValidationError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.to_owned().try_into()
    }
}
impl From<DirectoryCursorV1> for String {
    fn from(value: DirectoryCursorV1) -> Self {
        value.0
    }
}

/// Authority namespace of a binding's current management owner.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ManagementAuthorityClassV1 {
    /// Installation operator.
    Installation,
    /// Tenant principal.
    Tenant,
}
redacted_contract_debug!(ManagementAuthorityClassV1);

/// Current management owner; null on the binding means management was detached.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ManagementOwnerV1 {
    /// Principal responsible for managed updates.
    pub principal_id: PrincipalId,
    /// Authority namespace under which the owner manages this binding.
    pub authority_class: ManagementAuthorityClassV1,
}
redacted_contract_debug!(ManagementOwnerV1);

/// Typed, nonsecret durable authority facts. Expiry and effective access must
/// be evaluated separately; a recorded active credential is not an auth claim.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DirectoryEntryV1 {
    /// Tenant lifecycle record.
    Tenant {
        /// Tenant identity.
        tenant_id: TenantId,
        /// Observed record revision.
        revision_id: RevisionId,
        /// Recorded lifecycle.
        state: TenantState,
    },
    /// Immutable principal identity.
    Principal {
        /// Principal identity.
        principal_id: PrincipalId,
        /// Observed record revision.
        revision_id: RevisionId,
        /// External identity namespace.
        #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 256)))]
        issuer: String,
        /// Subject within the issuer namespace.
        #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 256)))]
        subject: String,
    },
    /// Principal-to-tenant grant and management ownership.
    Binding {
        /// Tenant identity.
        tenant_id: TenantId,
        /// Binding identity.
        binding_id: BindingId,
        /// Observed record revision.
        revision_id: RevisionId,
        /// Grant recipient.
        principal_id: PrincipalId,
        /// Recorded tenant role.
        role: TenantRole,
        /// Explicit grant scope.
        scope: GrantScope,
        /// Expiry ceiling; not a permanent active assertion.
        expires_at: Timestamp,
        /// Current managed writer, or explicit null after detachment.
        #[serde(deserialize_with = "nullable")]
        #[cfg_attr(
            feature = "schema",
            schemars(with = "super::resources::schema::RequiredNullable<ManagementOwnerV1>")
        )]
        management_owner: Option<ManagementOwnerV1>,
    },
    /// Durable credential record, including revoked and expired records.
    Credential {
        /// Tenant identity.
        tenant_id: TenantId,
        /// Nonsecret credential identity.
        credential_id: CredentialId,
        /// Observed record revision.
        revision_id: RevisionId,
        /// Authenticated principal identity.
        principal_id: PrincipalId,
        /// Bound grant, or explicit null for an audit reader.
        #[serde(deserialize_with = "nullable")]
        #[cfg_attr(
            feature = "schema",
            schemars(with = "super::resources::schema::RequiredNullable<BindingId>")
        )]
        binding_id: Option<BindingId>,
        /// Recorded permission ceiling.
        #[cfg_attr(feature = "schema", schemars(length(max = 10)))]
        permissions: Vec<Permission>,
        /// Recorded scope ceiling.
        scope: GrantScope,
        /// Expiry instant; readers derive expiration at their observation time.
        expires_at: Timestamp,
        /// Persisted lifecycle, without a clock-derived active guarantee.
        recorded_state: CredentialState,
        /// Receipt-only audit credential marker.
        audit_reader: bool,
    },
}
redacted_contract_debug!(DirectoryEntryV1);

impl DirectoryEntryV1 {
    /// Stable ASCII sort key; unscoped records have an empty tenant component.
    pub fn key(&self) -> (&str, &str, &str) {
        match self {
            Self::Binding {
                tenant_id,
                binding_id,
                ..
            } => ("binding", tenant_id.as_str(), binding_id.as_str()),
            Self::Credential {
                tenant_id,
                credential_id,
                ..
            } => ("credential", tenant_id.as_str(), credential_id.as_str()),
            Self::Principal { principal_id, .. } => ("principal", "", principal_id.as_str()),
            Self::Tenant { tenant_id, .. } => ("tenant", "", tenant_id.as_str()),
        }
    }

    /// Cursor immediately after this entry within the page's authority revision.
    pub fn cursor(&self) -> DirectoryCursorV1 {
        let (kind, tenant, id) = self.key();
        DirectoryCursorV1(format!("v1|{kind}|{tenant}|{id}"))
    }
}
impl Validate for DirectoryEntryV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Tenant { .. } => Ok(()),
            Self::Principal {
                issuer, subject, ..
            } => {
                text(issuer, 256)?;
                text(subject, 256)
            }
            Self::Binding { scope, .. } => scope.validate(),
            Self::Credential {
                scope,
                permissions,
                binding_id,
                audit_reader,
                ..
            } => {
                scope.validate()?;
                unique(permissions, usize::from(!*audit_reader), 10)?;
                require(
                    if *audit_reader {
                        binding_id.is_none() && permissions.is_empty()
                    } else {
                        binding_id.is_some()
                    },
                    "directory credential binding",
                )
            }
        }
    }
}

/// One revision-consistent operator directory page, at most 100 entries and 1 MiB.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DirectoryPageV1 {
    /// Wire schema discriminator, exactly 1.
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1)))]
    pub schema_version: u8,
    /// Addressed authority store.
    pub installation_id: InstallationId,
    /// Authority revision shared by every page in a complete scan.
    pub authority_revision: AuthorityRevision,
    /// Strictly increasing entries in ASCII `(kind, tenant_id, id)` order.
    #[cfg_attr(feature = "schema", schemars(length(max = 100)))]
    pub entries: Vec<DirectoryEntryV1>,
    /// Last returned entry's cursor when more entries remain; otherwise null.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "super::resources::schema::RequiredNullable<DirectoryCursorV1>")
    )]
    pub next_after: Option<DirectoryCursorV1>,
}
redacted_contract_debug!(DirectoryPageV1);

impl Validate for DirectoryPageV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.schema_version == 1 && self.entries.len() <= 100,
            "directory page bounds",
        )?;
        for entry in &self.entries {
            entry.validate()?;
        }
        require(
            self.entries
                .windows(2)
                .all(|pair| pair[0].key() < pair[1].key()),
            "directory ordering",
        )?;
        require(
            self.next_after.as_ref().is_none_or(|cursor| {
                self.entries
                    .last()
                    .is_some_and(|entry| entry.cursor() == *cursor)
            }),
            "directory continuation",
        )?;
        bytes(self, MAX_DIRECTORY_PAGE_BYTES)
    }
}
