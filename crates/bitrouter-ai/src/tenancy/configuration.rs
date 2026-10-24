//! Typed configuration-as-code: references and bounded framework settings only.

use serde::{Deserialize, Serialize};

use super::authority::{OwnerKind, ResourceKey, ResourceKind};
use super::identity::{
    AuthorityRevision, Digest, InstallationId, OperationId, OwnerId, ResourceId, RevisionId,
    TenantId,
};
use super::validation::{Validate, ValidationError, bytes, nullable, require, text};

/// An immutable resource revision, qualified by owner and resource kind.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRef {
    /// Owner namespace.
    pub owner_kind: OwnerKind,
    /// Owner identity in that namespace.
    pub owner_id: OwnerId,
    /// Resource class.
    pub resource_kind: ResourceKind,
    /// Stable resource identity.
    pub resource_id: ResourceId,
    /// Exact retained version; never an implicit latest reference.
    pub revision_id: RevisionId,
}
impl ResourceRef {
    /// Return the grant key for this immutable reference.
    pub fn key(&self) -> ResourceKey {
        ResourceKey {
            owner_kind: self.owner_kind,
            owner_id: self.owner_id.clone(),
            resource_kind: self.resource_kind,
            resource_id: self.resource_id.clone(),
        }
    }
}

/// Bounded generation defaults, without provider endpoints or host configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationDefaults {
    /// Maximum output tokens, at most one million.
    #[serde(deserialize_with = "super::number::integer")]
    pub max_output_tokens: u32,
    /// Finite sampling temperature from zero through two.
    pub temperature: f64,
}

/// Learning behavior selected by a tenant configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningMode {
    /// Do not initiate learning work.
    Off,
    /// Record observations without exploration.
    Observe,
    /// Permit bounded exploration under current grants.
    Explore,
}

/// Bounded learning settings, enforced by the service's scheduling feature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Learning {
    /// Selected learning behavior.
    pub mode: LearningMode,
    /// Maximum evaluation work, at most 100,000.
    #[serde(deserialize_with = "super::number::integer")]
    pub max_evaluations: u32,
}

/// Technical tenant admission limits, further bounded by installation limits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// At most 128 simultaneously active executions.
    #[serde(deserialize_with = "super::number::integer")]
    pub active_executions: u16,
    /// At most 256 queued work items.
    #[serde(deserialize_with = "super::number::integer")]
    pub queued_work: u16,
    /// At most eight learning jobs.
    #[serde(deserialize_with = "super::number::integer")]
    pub learning_jobs: u8,
}

/// Tenant framework settings; no arbitrary URL, path, environment or executable fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    /// Exact routing references; at most 256 references including the policy.
    pub routes: Vec<ResourceRef>,
    /// Optional exact policy reference. Omission is invalid; null means none.
    #[serde(deserialize_with = "nullable")]
    pub policy: Option<ResourceRef>,
    /// Generation defaults for this revision.
    pub generation_defaults: GenerationDefaults,
    /// Learning settings for this revision.
    pub learning: Learning,
    /// Requested technical admission limits.
    pub limits: Limits,
}

/// Prepare an immutable configuration under the observed authority revision.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationPut {
    /// Stable management command identity.
    pub operation_id: OperationId,
    /// Observed authority snapshot; no implicit substitution.
    pub expected_authority_revision: AuthorityRevision,
    /// Bounded configuration to validate and compile.
    pub document: Configuration,
    /// Addressed store identity.
    pub installation_id: InstallationId,
}

/// Stored immutable configuration with its compiler and digest identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationRevision {
    /// Resource ownership domain.
    pub tenant_id: TenantId,
    /// Identity of this immutable configuration revision.
    pub revision_id: RevisionId,
    /// Contract schema version, currently one.
    #[serde(deserialize_with = "super::number::integer")]
    pub schema_version: u32,
    /// Compiler identity, at most 64 characters.
    pub compiler_version: String,
    /// Digest of the retained configuration, never an authorization key.
    pub content_digest: Digest,
    /// The exact retained document.
    pub document: Configuration,
}

/// CAS activation or rollback to an already retained revision.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    /// Stable management command identity.
    pub operation_id: OperationId,
    /// Null only when no revision is active; omission is invalid.
    #[serde(deserialize_with = "nullable")]
    pub expected_active_revision: Option<RevisionId>,
    /// Exact retained revision to activate.
    pub revision_id: RevisionId,
    /// Observed current authority snapshot.
    pub expected_authority_revision: AuthorityRevision,
    /// Addressed store identity.
    pub installation_id: InstallationId,
}

/// Current tenant activation pointer, readable only under current tenant-wide authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveRevision {
    /// Resource ownership domain.
    pub tenant_id: TenantId,
    /// Active revision, or explicit null when none exists.
    #[serde(deserialize_with = "nullable")]
    pub revision_id: Option<RevisionId>,
    /// Current authority snapshot.
    pub authority_revision: AuthorityRevision,
}

impl Validate for Configuration {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.routes
                .len()
                .saturating_add(usize::from(self.policy.is_some()))
                <= 256,
            "configuration reference bound",
        )?;
        require(
            self.routes
                .iter()
                .all(|r| r.resource_kind == ResourceKind::Route)
                && self
                    .policy
                    .as_ref()
                    .is_none_or(|p| p.resource_kind == ResourceKind::Policy),
            "configuration reference kind",
        )?;
        let mut keys = std::collections::HashSet::new();
        for reference in self.routes.iter().chain(self.policy.iter()) {
            require(
                keys.insert((
                    &reference.owner_kind,
                    &reference.owner_id,
                    &reference.resource_kind,
                    &reference.resource_id,
                )),
                "multiple revisions of one resource",
            )?;
        }
        require(
            self.generation_defaults.max_output_tokens <= 1_000_000
                && self.generation_defaults.temperature.is_finite()
                && (0.0..=2.0).contains(&self.generation_defaults.temperature),
            "generation bounds",
        )?;
        require(self.learning.max_evaluations <= 100_000, "learning bound")?;
        require(
            self.limits.active_executions <= 128
                && self.limits.queued_work <= 256
                && self.limits.learning_jobs <= 8,
            "admission bounds",
        )?;
        bytes(self, 1_048_576)
    }
}
impl Validate for ConfigurationPut {
    fn validate(&self) -> Result<(), ValidationError> {
        self.document.validate()
    }
}
impl Validate for ConfigurationRevision {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 1, "configuration schema version")?;
        text(&self.compiler_version, 64)?;
        self.document.validate()
    }
}
impl Validate for Activation {
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}
impl Validate for ActiveRevision {
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}
