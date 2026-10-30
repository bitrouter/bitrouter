//! Actual live parent ownership. Persisted source data cannot construct this enum.
use crate::tenancy::{
    Service, capture,
    execution::{host, session},
    model::Snapshot,
    transaction::Proof,
};
use bitrouter_ai::tenancy::operation::ErrorCode;
use std::sync::Arc;

#[derive(Clone)]
pub(in crate::tenancy) enum Binding {
    Capture(Arc<capture::Binding>),
    Native(Arc<session::Binding>),
}
impl Binding {
    pub(in crate::tenancy) fn service(&self) -> &Service {
        self.authority().service()
    }
    pub(in crate::tenancy) fn authority(&self) -> &host::Authority {
        match self {
            Self::Capture(binding) => binding.authority(),
            Self::Native(binding) => binding.authority().host(),
        }
    }
    pub(in crate::tenancy) fn origin(&self, proof: &Arc<Proof>) -> bool {
        match self {
            Self::Capture(binding) => binding.origin(proof),
            Self::Native(binding) => binding.authority().origin(proof),
        }
    }
    pub(in crate::tenancy) fn assessment_check(&self) -> Result<(), ErrorCode> {
        match self {
            Self::Capture(binding) => binding.assessment_check(),
            Self::Native(binding) => binding.assessment_check(),
        }
    }
    pub(in crate::tenancy) fn assessment_in(&self, state: &Snapshot) -> Result<(), ErrorCode> {
        match self {
            Self::Capture(binding) => binding.assessment_in(state),
            Self::Native(binding) => binding.assessment_in(state),
        }
    }
}
