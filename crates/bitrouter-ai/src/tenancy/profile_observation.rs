//! Bounded advisory reads of exact retained admission profiles, without routes.
//!
//! Observations never allocate admission state, reserve capacity or grant dispatch.

use serde::{Deserialize, Serialize};

use super::{
    admission::{AvailabilityObservation, HealthObservation, RateObservationV1},
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    identity::{AuthorityRevision, InstallationId, InstanceId, RevisionId, Timestamp},
    validation::{Validate, ValidationError, bytes, nullable, require, unique},
};

/// Maximum number of distinct full profile references in one read.
pub const MAX_PROFILES: usize = 256;
/// Maximum original JSON bytes in each request or response.
pub const MAX_BYTES: usize = 1024 * 1024;

/// Explicit discovery for the separately versioned profile observation reader.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AdmissionProfileObservationCapabilitiesV1 {
    /// Discovery version, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1)))]
    pub contract_version: u32,
    /// Persistent authority installation.
    pub installation_id: InstallationId,
    /// Actual process incarnation.
    pub instance_id: InstanceId,
    /// Authority observed during discovery, without a lease or reservation.
    pub authority_revision: AuthorityRevision,
    /// Whether this service supports observations.
    pub observation_enabled: bool,
    /// Maximum references per explicitly submitted batch.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 256)))]
    pub max_profiles: u32,
    /// Maximum original request JSON bytes.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1048576)))]
    pub max_request_bytes: u32,
    /// Maximum original response JSON bytes.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1048576)))]
    pub max_response_bytes: u32,
    /// Supported retained profile versions, exactly [1, 2].
    #[serde(deserialize_with = "profile_versions")]
    #[cfg_attr(feature = "schema", schemars(extend("const" = serde_json::json!([1, 2]))))]
    pub profile_versions: [u8; 2],
}

fn profile_versions<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[u8; 2], D::Error> {
    #[derive(Deserialize)]
    struct Version(#[serde(deserialize_with = "super::number::integer")] u8);
    let [first, second] = <[Version; 2]>::deserialize(d)?;
    Ok([first.0, second.0])
}

impl Validate for AdmissionProfileObservationCapabilitiesV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.contract_version == 1
                && (1..=256).contains(&self.max_profiles)
                && (1..=1048576).contains(&self.max_request_bytes)
                && (1..=1048576).contains(&self.max_response_bytes)
                && self.profile_versions == [1, 2],
            "profile observation capabilities",
        )
    }
}

/// One operator-only query, bound to a previously observed authority revision.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AdmissionProfileObservationRequestV1 {
    /// Query version, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1)))]
    pub schema_version: u32,
    /// Reject the whole query if actual authority differs.
    pub expected_authority_revision: AuthorityRevision,
    /// Distinct full operator-pool Policy references, in desired response order.
    #[cfg_attr(feature = "schema", schemars(schema_with = "schema::references"))]
    pub profiles: Vec<ResourceRef>,
}

fn profile_ref(reference: &ResourceRef) -> Result<(), ValidationError> {
    require(
        reference.owner_kind == OwnerKind::OperatorPool
            && reference.resource_kind == ResourceKind::Policy,
        "profile observation reference",
    )
}

impl Validate for AdmissionProfileObservationRequestV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 1, "profile observation version")?;
        unique(&self.profiles, 1, MAX_PROFILES)?;
        for reference in &self.profiles {
            profile_ref(reference)?;
        }
        bytes(self, MAX_BYTES)
    }
}

/// One process's complete ordered batch, sampled under one operational guard.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AdmissionProfileObservationSnapshotV1 {
    /// Response version, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1)))]
    pub schema_version: u32,
    /// Actual persistent installation.
    pub installation_id: InstallationId,
    /// Process incarnation; consumers compare their actual discovery and freshness.
    pub instance_id: InstanceId,
    /// Authority revision that remained unchanged throughout the query.
    pub authority_revision: AuthorityRevision,
    /// UTC time at batch sampling, not an extension of any caller deadline.
    pub observed_at: Timestamp,
    /// Exactly the request references, cardinality and order; no omitted entries.
    #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 256)))]
    pub profiles: Vec<ProfileObservationV1>,
}

impl Validate for AdmissionProfileObservationSnapshotV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 1, "profile observation version")?;
        require(
            (1..=MAX_PROFILES).contains(&self.profiles.len()),
            "profile observation count",
        )?;
        for (index, entry) in self.profiles.iter().enumerate() {
            entry.validate()?;
            require(
                !self.profiles[..index]
                    .iter()
                    .any(|previous| previous.profile == entry.profile),
                "duplicate profile observation",
            )?;
        }
        bytes(self, MAX_BYTES)
    }
}

/// Advisory state for one exact pin, using the current same-key profile head.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ProfileObservationV1 {
    /// Original requested pin, never replaced by its current head.
    #[cfg_attr(feature = "schema", schemars(schema_with = "schema::reference"))]
    pub profile: ResourceRef,
    /// Current head; required null means profile authority could not be established.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "super::resources::schema::RequiredNullable<RevisionId>")
    )]
    pub current_profile_revision: Option<RevisionId>,
    /// Current ceiling and actual charged count; required null when unconfigured or denied.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "super::resources::schema::RequiredNullable<RateObservationV1>")
    )]
    pub rate: Option<RateObservationV1>,
    /// Actual explicit observation-group count; absent configuration is required null.
    #[serde(deserialize_with = "super::number::nullable_integer")]
    #[cfg_attr(
        feature = "schema",
        schemars(
            with = "super::resources::schema::RequiredNullable<u64>",
            extend("maximum" = 9007199254740991_u64)
        )
    )]
    pub observed_shared_count: Option<u64>,
    /// Existing version-specific local or shared circuit observation.
    pub health: HealthObservation,
    /// Advisory priority: circuit refusal, rate refusal, capacity refusal, eligible.
    pub availability: AvailabilityObservation,
}

impl Validate for ProfileObservationV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        profile_ref(&self.profile)?;
        if self.availability == AvailabilityObservation::AuthorityDenied {
            return require(
                self.current_profile_revision.is_none()
                    && self.rate.is_none()
                    && self.observed_shared_count.is_none()
                    && self.health == HealthObservation::Unavailable,
                "denied profile observation",
            );
        }
        require(
            self.current_profile_revision.is_some()
                && self.health != HealthObservation::Unavailable,
            "profile observation authority",
        )?;
        if let Some(rate) = &self.rate {
            require(
                rate.shared_count <= AuthorityRevision::MAX,
                "observed rate count",
            )?;
        }
        if let Some(count) = self.observed_shared_count {
            require(count <= AuthorityRevision::MAX, "observed group count")?;
            require(
                self.rate
                    .as_ref()
                    .is_none_or(|rate| rate.shared_count == count),
                "conflicting observed group count",
            )?;
        }
        let blocked = self.health == HealthObservation::Blocked;
        let limited = self
            .rate
            .as_ref()
            .is_some_and(|rate| rate.shared_count >= u64::from(rate.member_rpm));
        let consistent = match self.availability {
            AvailabilityObservation::CircuitBlocked => blocked,
            AvailabilityObservation::RateLimited => !blocked && limited,
            AvailabilityObservation::Eligible | AvailabilityObservation::CapacityDenied => {
                !blocked && !limited
            }
            AvailabilityObservation::AuthorityDenied => false,
        };
        require(consistent, "profile observation availability")
    }
}

#[cfg(feature = "schema")]
mod schema {
    use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};

    pub(super) fn reference(generator: &mut SchemaGenerator) -> Schema {
        let mut schema = super::ResourceRef::json_schema(generator);
        schema.insert(
            "allOf".into(),
            serde_json::json!([{
                "properties": {
                    "owner_kind": {"const":"operator_pool"},
                    "resource_kind": {"const":"policy"}
                }
            }]),
        );
        schema
    }

    pub(super) fn references(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"array", "minItems":1, "maxItems":256,
            "uniqueItems":true, "items": reference(generator)})
    }
}
