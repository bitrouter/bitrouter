//! Borrowed operational access to the current admission profile.

use bitrouter_ai::tenancy::{
    admission::{AdmissionProfile, FallbackRule, RateProfileV1},
    identity::ResourceId,
};

#[derive(Clone, Copy)]
pub(in crate::tenancy) struct View<'a>(pub(in crate::tenancy) &'a AdmissionProfile);

impl<'a> From<&'a AdmissionProfile> for View<'a> {
    fn from(profile: &'a AdmissionProfile) -> Self {
        Self(profile)
    }
}

impl<'a> View<'a> {
    pub(in crate::tenancy) fn rate(self) -> Option<&'a RateProfileV1> {
        self.0.rate.as_ref()
    }
    pub(in crate::tenancy) fn observed_group(self) -> Option<&'a ResourceId> {
        self.0.observe_rate_group_id.as_ref()
    }
    pub(in crate::tenancy) fn fallback(self) -> FallbackRule {
        self.0.fallback
    }
}
