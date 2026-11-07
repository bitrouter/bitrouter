//! Explicit operational admission contracts. These values grant no authority.
//!
//! Rate windows and circuit state belong to one service process. Observations
//! are advisory and are never execution receipts or proof of provider work.

use serde::{Deserialize, Serialize};

use super::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    identity::{InstanceId, ResourceId, RevisionId, Timestamp},
    ordered::{GenerationProtocol, MAX_CANDIDATES},
    validation::{Validate, ValidationError, bytes, nullable, require},
};

/// Enforcement selected explicitly by an ordered caller and checked by the service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AdmissionMode {
    /// Existing static ordered execution without operational profiles.
    Legacy,
    /// Every actual candidate is bound to an installation-managed profile.
    ProfiledV1,
}

/// Immutable rules with a live, independently rotatable member RPM ceiling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AdmissionProfileV1 {
    /// Profile format, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1)))]
    pub schema_version: u8,
    /// Null disables rate admission; a zero ceiling refuses locally.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "super::resources::schema::RequiredNullable<RateProfileV1>")
    )]
    pub rate: Option<RateProfileV1>,
    /// Optional read-only group in this profile's operator-pool namespace.
    /// Does not enable rate admission, charge a timestamp or consult a circuit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observe_rate_group_id: Option<ResourceId>,
    /// Null disables the availability circuit.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "super::resources::schema::RequiredNullable<HealthProfileV1>")
    )]
    pub health: Option<HealthProfileV1>,
    /// Explicit permission to advance after a proven terminal HTTP rejection.
    pub fallback: FallbackRule,
}

/// Shared sliding-window identity and this member's current ceiling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RateProfileV1 {
    /// Scoped by installation and operator-pool owner, independent of revisions.
    pub rate_group_id: ResourceId,
    /// Maximum reached entries in the shared 60-second window; zero is valid.
    #[serde(deserialize_with = "super::number::integer")]
    pub member_rpm: u32,
}

/// Circuit rules, immutable for a profile key, including across RPM rotations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct HealthProfileV1 {
    /// Closed classification of actual provider terminal outcomes.
    pub rules: HealthRules,
    /// Consecutive failures needed to open the circuit; at least one.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    pub failure_threshold: u32,
    /// Transient cooldown in seconds; zero is valid.
    #[serde(deserialize_with = "super::number::full_integer")]
    pub transient_cooldown_seconds: u64,
    /// Persistent cooldown in seconds, at least the transient cooldown.
    #[serde(deserialize_with = "super::number::full_integer")]
    pub persistent_cooldown_seconds: u64,
    /// Exclusive probe lease in seconds; at least one.
    #[serde(deserialize_with = "super::number::full_integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    pub probe_lease_seconds: u64,
}

/// Versioned provider availability classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HealthRules {
    /// Protocol terminal success, persistent authorization failures and transient failures.
    AvailabilityV1,
}

/// Ordered fallback permission; uncertainty never authorizes another attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FallbackRule {
    /// Stop after any reached provider attempt.
    Stop,
    /// Advance only after an explicitly qualified terminal HTTP rejection.
    OrderedTerminalRejectionsV1,
}

impl Validate for AdmissionProfileV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 1, "admission profile version")?;
        if let (Some(rate), Some(observed)) = (&self.rate, &self.observe_rate_group_id) {
            require(
                &rate.rate_group_id == observed,
                "conflicting observed rate group",
            )?;
        }
        if let Some(health) = &self.health {
            require(
                health.failure_threshold > 0
                    && health.probe_lease_seconds > 0
                    && health.persistent_cooldown_seconds >= health.transient_cooldown_seconds,
                "admission health rules",
            )?;
        }
        bytes(self, 4096)
    }
}

impl AdmissionProfileV1 {
    /// Whether a new live revision preserves all rules except the RPM ceiling.
    pub fn permits_rotation_to(&self, next: &Self) -> bool {
        self.schema_version == next.schema_version
            && self.health == next.health
            && self.fallback == next.fallback
            && self.observe_rate_group_id == next.observe_rate_group_id
            && self.rate.as_ref().map(|rate| &rate.rate_group_id)
                == next.rate.as_ref().map(|rate| &rate.rate_group_id)
    }
}

/// Exact association with one retained protocol-root ordinal and credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AdmissionAttachmentV1 {
    /// Actual ingress protocol, not the target's upstream protocol.
    #[serde(deserialize_with = "legacy_protocol")]
    #[cfg_attr(feature = "schema", schemars(with = "LegacyGenerationProtocol"))]
    pub inbound_protocol: GenerationProtocol,
    /// Original root ordinal, below 256.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(max = 255)))]
    pub candidate_index: u16,
    /// Exact retained credential revision for this root entry.
    pub credential: ResourceRef,
    /// Pinned operator-pool Policy revision containing an admission profile.
    pub profile: ResourceRef,
}

impl Validate for AdmissionAttachmentV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.inbound_protocol != GenerationProtocol::GenerateContent,
            "v1 admission protocol",
        )?;
        require(
            usize::from(self.candidate_index) < MAX_CANDIDATES,
            "admission ordinal",
        )?;
        require(
            self.credential.resource_kind == ResourceKind::Credential,
            "admission credential",
        )?;
        validate_profile_reference(&self.profile)
    }
}

pub(super) fn validate_profile_reference(reference: &ResourceRef) -> Result<(), ValidationError> {
    require(
        reference.resource_kind == ResourceKind::Policy
            && reference.owner_kind == OwnerKind::OperatorPool,
        "admission profile reference",
    )
}

/// One process's advisory view; no observations reserve rate capacity or probes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RuntimeObservationV1 {
    /// Process incarnation whose local counters were inspected.
    pub instance_id: InstanceId,
    /// UTC observation time. Producers use whole seconds and the `Z` suffix.
    pub observed_at: Timestamp,
    /// Exactly the descriptor ordinals, in descriptor order.
    pub candidates: Vec<CandidateObservationV1>,
}

/// Advisory eligibility of one retained candidate under current authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CandidateObservationV1 {
    /// Original protocol-root ordinal.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(max = 255)))]
    pub candidate_index: u16,
    /// Current same-key profile head; null means authority is unavailable.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "super::resources::schema::RequiredNullable<RevisionId>")
    )]
    pub current_profile_revision: Option<RevisionId>,
    /// Null when disabled or unavailable, distinguished by authority and health.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "super::resources::schema::RequiredNullable<RateObservationV1>")
    )]
    pub rate: Option<RateObservationV1>,
    /// Count for an explicit read-only group association, without a ceiling.
    /// Absence is unavailable or unconfigured, never a measured zero.
    #[serde(
        default,
        deserialize_with = "super::number::nullable_integer",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema", schemars(range(max = 9007199254740991_u64)))]
    pub observed_shared_count: Option<u64>,
    /// Current circuit observation, without lease tokens or retained history.
    pub health: HealthObservation,
    /// Advisory availability; execution always checks again at the reached hop.
    pub availability: AvailabilityObservation,
}

/// Observed shared count and this member's current ceiling.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RateObservationV1 {
    /// Current live member ceiling; zero is a valid local refusal.
    #[serde(deserialize_with = "super::number::integer")]
    pub member_rpm: u32,
    /// Shared count, decoded as an exact JSON integer.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(max = 9007199254740991_u64)))]
    pub shared_count: u64,
}

/// Compact circuit state without operational authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HealthObservation {
    /// The current profile explicitly disables health admission.
    Disabled,
    /// The circuit is closed.
    Healthy,
    /// Cooldown or another active probe blocks admission.
    Blocked,
    /// A probe could be claimed by a subsequent actual hop.
    ProbeReady,
    /// Current authority or state could not be established.
    Unavailable,
}

/// Compact eligibility result; no result promises later admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AvailabilityObservation {
    /// Eligible at the observation instant.
    Eligible,
    /// Shared count reaches this member's ceiling.
    RateLimited,
    /// Circuit cooldown or an active probe blocks admission.
    CircuitBlocked,
    /// Current profile authority is missing, revoked or otherwise invalid.
    AuthorityDenied,
    /// Bounded operational state cannot admit another entry.
    CapacityDenied,
}

// The new private ingress must not widen format-one resource authority or schemas.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyGenerationProtocol {
    ChatCompletions,
    Messages,
    Responses,
}

fn legacy_protocol<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<GenerationProtocol, D::Error> {
    Ok(match LegacyGenerationProtocol::deserialize(deserializer)? {
        LegacyGenerationProtocol::ChatCompletions => GenerationProtocol::ChatCompletions,
        LegacyGenerationProtocol::Messages => GenerationProtocol::Messages,
        LegacyGenerationProtocol::Responses => GenerationProtocol::Responses,
    })
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for LegacyGenerationProtocol {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "GenerationProtocol".into()
    }
    fn schema_id() -> std::borrow::Cow<'static, str> {
        "bitrouter_ai::tenancy::ordered::GenerationProtocol".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"string","enum":["chat_completions","messages","responses"]})
    }
}
