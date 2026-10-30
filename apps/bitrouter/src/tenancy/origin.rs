//! Actual exclusive acquisition shared by bootstrap and serving recovery.
use super::{ownership::Claim, transaction::Proof};
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::identity::InstanceId;
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Data provenance, not an execution or recovery capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Stamp {
    acquisition: String,
    instance: InstanceId,
}
impl Stamp {
    pub(in crate::tenancy) fn validate(&self) -> Result<()> {
        uuid::Uuid::parse_str(&self.acquisition)?;
        Ok(())
    }
}

pub(super) struct StartupOwner {
    _claim: Claim,
    stamp: Stamp,
    proof: Arc<Proof>,
    ready: AtomicBool,
}
impl StartupOwner {
    pub(super) fn new(claim: Claim, instance: InstanceId, proof: Arc<Proof>) -> Arc<Self> {
        Arc::new(Self {
            stamp: Stamp {
                acquisition: claim.identity().to_owned(),
                instance,
            },
            _claim: claim,
            proof,
            ready: AtomicBool::new(false),
        })
    }
    /// Worker-local provenance before readiness; this returns data only and
    /// cannot construct Service, Binding, execution or a dispatch capability.
    pub(super) fn bootstrap_stamp(&self, proof: &Arc<Proof>) -> Result<Stamp> {
        ensure!(
            Arc::ptr_eq(&self.proof, proof),
            "bootstrap origin differs from actual H owner"
        );
        self.stamp.validate()?;
        Ok(self.stamp.clone())
    }
    pub(super) fn publish(&self, proof: &Arc<Proof>) -> Result<()> {
        self.bootstrap_stamp(proof)?;
        proof.ensure_clean()?;
        self.ready.store(true, Ordering::Release);
        Ok(())
    }
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(super) fn stamp(&self, proof: &Arc<Proof>) -> Result<Stamp> {
        ensure!(
            Arc::ptr_eq(&self.proof, proof) && self.ready.load(Ordering::Acquire),
            "startup origin is not ready"
        );
        self.bootstrap_stamp(proof)
    }
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(super) fn excludes(&self, proof: &Arc<Proof>, previous: &Stamp) -> Result<()> {
        self.stamp(proof)?;
        self.bootstrap_excludes(proof, previous)
    }
    // The paid bootstrap participant can reuse this same claimed pool origin.
    // This method grants no Service readiness, current scope or effect permission.
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(super) fn bootstrap_excludes(&self, proof: &Arc<Proof>, previous: &Stamp) -> Result<()> {
        previous.validate()?;
        let current = self.bootstrap_stamp(proof)?;
        if current.acquisition == previous.acquisition {
            return Err(crate::tenancy::session::Rejected(
                bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch,
            )
            .into());
        }
        Ok(())
    }
}
