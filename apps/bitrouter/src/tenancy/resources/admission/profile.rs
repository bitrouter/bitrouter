//! Private views preserve each profile's original closed wire representation.

use bitrouter_ai::tenancy::{
    admission::{AdmissionProfileV1, FallbackRule, HealthProfileV1, RateProfileV1},
    admission_v2::AdmissionProfileV2,
    identity::ResourceId,
};

#[derive(Clone)]
pub(in crate::tenancy) enum Profile {
    Local(AdmissionProfileV1),
    Shared(AdmissionProfileV2),
}

#[derive(Clone, Copy)]
pub(in crate::tenancy) enum View<'a> {
    Local(&'a AdmissionProfileV1),
    Shared(&'a AdmissionProfileV2),
}

impl<'a> From<&'a AdmissionProfileV1> for View<'a> {
    fn from(profile: &'a AdmissionProfileV1) -> Self {
        Self::Local(profile)
    }
}

impl<'a> From<&'a AdmissionProfileV2> for View<'a> {
    fn from(profile: &'a AdmissionProfileV2) -> Self {
        Self::Shared(profile)
    }
}

impl<'a> From<&'a Profile> for View<'a> {
    fn from(profile: &'a Profile) -> Self {
        match profile {
            Profile::Local(profile) => Self::Local(profile),
            Profile::Shared(profile) => Self::Shared(profile),
        }
    }
}

impl<'a> View<'a> {
    pub(in crate::tenancy) fn rate(self) -> Option<&'a RateProfileV1> {
        match self {
            Self::Local(profile) => profile.rate.as_ref(),
            Self::Shared(profile) => profile.rate.as_ref(),
        }
    }

    pub(in crate::tenancy) fn observed_group(self) -> Option<&'a ResourceId> {
        match self {
            Self::Local(profile) => profile.observe_rate_group_id.as_ref(),
            Self::Shared(profile) => profile.observe_rate_group_id.as_ref(),
        }
    }

    pub(in crate::tenancy) fn health(self) -> Option<&'a HealthProfileV1> {
        match self {
            Self::Local(profile) => profile.health.as_ref(),
            Self::Shared(profile) => profile.health.as_ref(),
        }
    }

    pub(in crate::tenancy) fn fallback(self) -> FallbackRule {
        match self {
            Self::Local(profile) => profile.fallback,
            Self::Shared(profile) => profile.fallback,
        }
    }
}

impl Profile {
    pub(super) fn permits_rotation_to(&self, next: View<'_>) -> bool {
        match (self, next) {
            (Self::Local(previous), View::Local(next)) => previous.permits_rotation_to(next),
            (Self::Shared(previous), View::Shared(next)) => previous.permits_rotation_to(next),
            _ => false,
        }
    }
}
