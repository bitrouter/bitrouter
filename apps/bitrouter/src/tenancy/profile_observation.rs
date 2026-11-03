//! Exact-profile reads share retained authority and the existing pure sampler.

pub(super) mod http;

use std::{io::Write, sync::Arc};

use bitrouter_ai::tenancy::{
    admission::{AvailabilityObservation, HealthObservation},
    identity::{AuthorityRevision, InstallationId},
    operation::ErrorCode,
    profile_observation::{
        AdmissionProfileObservationCapabilitiesV1, AdmissionProfileObservationRequestV1,
        AdmissionProfileObservationSnapshotV1, MAX_BYTES, ProfileObservationV1,
    },
    validation::Validate,
};
use chrono::Utc;
use serde::Serialize;
use tokio::{sync::SemaphorePermit, time::Instant};

use super::{
    Service,
    error::Failure,
    model::{Caller, Snapshot},
    operational::state::Denial,
};

/// Owns bounded read capacity and one immutable authority view, not its lock.
pub(in crate::tenancy) struct Read<'a> {
    service: &'a Service,
    caller: Caller,
    state: Arc<Snapshot>,
    deadline: Instant,
    _permit: SemaphorePermit<'a>,
}

fn deadline(at: Instant) -> Result<(), Failure> {
    if Instant::now() >= at {
        Err(ErrorCode::CapacityExceeded.into())
    } else {
        Ok(())
    }
}

impl Service {
    fn profile_observation_current(&self) -> Result<Arc<Snapshot>, Failure> {
        self.authority
            .try_snapshot()
            .map(Arc::new)
            .map_err(Failure::from)
    }

    pub(in crate::tenancy) fn profile_observation_read(
        &self,
        caller: &Caller,
        installation: Option<&InstallationId>,
        at: Instant,
    ) -> Result<Read<'_>, Failure> {
        deadline(at)?;
        if !matches!(caller, Caller::Installation(_)) {
            return Err(ErrorCode::Forbidden.into());
        }
        let permit = self.read_permit()?;
        let state = self.profile_observation_current()?;
        state.principal(caller, Utc::now()).map_err(Failure::from)?;
        if installation.is_some_and(|expected| *expected != state.installation) {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        deadline(at)?;
        Ok(Read {
            service: self,
            caller: caller.clone(),
            state,
            deadline: at,
            _permit: permit,
        })
    }
}

impl Read<'_> {
    pub(in crate::tenancy) fn capabilities(&self) -> Result<Vec<u8>, Failure> {
        self.finish(&AdmissionProfileObservationCapabilitiesV1 {
            contract_version: 1,
            installation_id: self.state.installation.clone(),
            instance_id: self.service.instance.clone(),
            authority_revision: self.state.revision,
            observation_enabled: true,
            max_profiles: 256,
            max_request_bytes: 1048576,
            max_response_bytes: 1048576,
            profile_versions: [1, 2],
        })
    }

    pub(in crate::tenancy) fn sample(
        &self,
        request: &AdmissionProfileObservationRequestV1,
    ) -> Result<AdmissionProfileObservationSnapshotV1, Failure> {
        deadline(self.deadline)?;
        request
            .validate()
            .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
        self.revision(request.expected_authority_revision)?;
        let mut profiles = Vec::new();
        profiles
            .try_reserve_exact(request.profiles.len())
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        // This guard never overlaps authority reacquisition or asynchronous I/O.
        let runtime = self.service.operational.try_snapshot().map_err(|denial| {
            Failure::from(if denial == Denial::Capacity {
                ErrorCode::CapacityExceeded
            } else {
                ErrorCode::AuthorityUnavailable
            })
        })?;
        let observed_at = Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            .try_into()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        for reference in &request.profiles {
            deadline(self.deadline)?;
            let mut entry = ProfileObservationV1 {
                profile: reference.clone(),
                current_profile_revision: None,
                rate: None,
                observed_shared_count: None,
                health: HealthObservation::Unavailable,
                availability: AvailabilityObservation::AuthorityDenied,
            };
            if let Some((current, rules)) = self.state.resource_admission.current_profile(reference)
            {
                let view = runtime.observe(&self.state.installation, &current, rules);
                entry.current_profile_revision = Some(current.revision_id);
                entry.rate = view.rate;
                entry.observed_shared_count = view.observed_shared_count;
                entry.health = view.health;
                entry.availability = view.availability;
            }
            profiles.push(entry);
        }
        drop(runtime);
        deadline(self.deadline)?;
        Ok(AdmissionProfileObservationSnapshotV1 {
            schema_version: 1,
            installation_id: self.state.installation.clone(),
            instance_id: self.service.instance.clone(),
            authority_revision: self.state.revision,
            observed_at,
            profiles,
        })
    }

    fn revision(&self, expected: AuthorityRevision) -> Result<(), Failure> {
        if self.state.revision == expected {
            Ok(())
        } else {
            Err(ErrorCode::RevisionConflict.into())
        }
    }

    pub(in crate::tenancy) fn finish<T: Serialize + Validate>(
        &self,
        value: &T,
    ) -> Result<Vec<u8>, Failure> {
        deadline(self.deadline)?;
        value
            .validate()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        let mut output = Bounded(Vec::new());
        output
            .0
            .try_reserve_exact(MAX_BYTES)
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        serde_json::to_writer(&mut output, value)
            .map_err(|_| Failure::from(ErrorCode::PayloadTooLarge))?;
        // Recheck after encoding, with no operational lock held. A changed
        // authority invalidates the complete batch rather than rebasing it.
        let current = self.service.profile_observation_current()?;
        current
            .principal(&self.caller, Utc::now())
            .map_err(Failure::from)?;
        if current.installation != self.state.installation {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        if current.revision != self.state.revision {
            return Err(ErrorCode::RevisionConflict.into());
        }
        deadline(self.deadline)?;
        Ok(output.0)
    }
}

struct Bounded(Vec<u8>);
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other(
                "profile observation response byte limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
