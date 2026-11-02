//! Explicit operator-pool shared health, independent of member rate ceilings.

use serde::{Deserialize, Serialize};

use super::{
    admission::{FallbackRule, HealthProfileV1, RateProfileV1},
    identity::ResourceId,
    validation::{Validate, ValidationError, bytes, nullable, require},
};

/// Immutable shared-health membership with an independently rotatable member RPM.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AdmissionProfileV2 {
    /// Profile format, exactly two. V1 retains profile-local health semantics.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 2, max = 2)))]
    pub schema_version: u8,
    /// Null disables charging; a zero member ceiling refuses locally.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "super::resources::schema::RequiredNullable<RateProfileV1>")
    )]
    pub rate: Option<RateProfileV1>,
    /// Optional read-only rate group in this profile's operator pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observe_rate_group_id: Option<ResourceId>,
    /// Null disables health admission and observation. Requires a null group.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "super::resources::schema::RequiredNullable<HealthProfileV1>")
    )]
    pub health: Option<HealthProfileV1>,
    /// Installation/operator-pool-scoped circuit identity; present exactly when health is enabled.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "super::resources::schema::RequiredNullable<ResourceId>")
    )]
    pub health_group_id: Option<ResourceId>,
    /// Explicit permission to advance after a qualified terminal rejection.
    pub fallback: FallbackRule,
}

impl Validate for AdmissionProfileV2 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 2, "admission profile version")?;
        require(
            self.health.is_some() == self.health_group_id.is_some(),
            "admission health group association",
        )?;
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

impl AdmissionProfileV2 {
    /// Whether a same-key successor preserves every rule except member RPM.
    pub fn permits_rotation_to(&self, next: &Self) -> bool {
        self.schema_version == next.schema_version
            && self.health_group_id == next.health_group_id
            && self.health == next.health
            && self.fallback == next.fallback
            && self.observe_rate_group_id == next.observe_rate_group_id
            && self.rate.as_ref().map(|rate| &rate.rate_group_id)
                == next.rate.as_ref().map(|rate| &rate.rate_group_id)
    }
}
