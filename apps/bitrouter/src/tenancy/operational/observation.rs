//! Manifest observations are advisory reads of current authority and local
//! counters. They neither reserve capacity nor enter accepted-selection hashes.

use bitrouter_ai::tenancy::{
    admission::{
        AdmissionMode, AvailabilityObservation, CandidateObservationV1, HealthObservation,
        RuntimeObservationV1,
    },
    operation::ErrorCode,
    ordered::CandidateManifestV1,
    validation::Validate,
};

use crate::tenancy::{Service, model::Snapshot};

impl Service {
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(crate) fn bind_operational_exporter(
        &self,
        exporter: std::sync::Arc<crate::telemetry::exporter::OtelExporter>,
    ) {
        // This private one-time startup binding shares the existing process
        // exporter. Exporter availability is never an admission prerequisite.
        if self.operational_exporter.set(exporter).is_err() {
            tracing::warn!("tenant operational exporter was already bound");
        }
    }

    pub(in crate::tenancy) fn observe_manifest(
        &self,
        state: &Snapshot,
        manifest: &mut CandidateManifestV1,
    ) -> Result<(), ErrorCode> {
        if manifest.admission_mode != AdmissionMode::ProfiledV1 {
            return Ok(());
        }
        let operational = self.operational.snapshot();
        let observed_at = chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            .try_into()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        let candidates = manifest
            .candidates
            .iter()
            .map(|candidate| {
                let denied = || CandidateObservationV1 {
                    candidate_index: candidate.candidate_index,
                    current_profile_revision: None,
                    rate: None,
                    observed_shared_count: None,
                    health: HealthObservation::Unavailable,
                    availability: AvailabilityObservation::AuthorityDenied,
                };
                let Some(pinned) = &candidate.admission_profile else {
                    return denied();
                };
                if state.resource_lifecycle.revoked(&manifest.route)
                    || state.resource_lifecycle.revoked(&candidate.credential)
                    || state.resource_lifecycle.revoked(pinned)
                    || state.resource_admission.profile(pinned).is_none()
                {
                    return denied();
                }
                let Some((current, rules)) = state.resource_admission.current_profile(pinned)
                else {
                    return denied();
                };
                let revision = &current.revision_id;
                match operational
                    .as_ref()
                    .map(|snapshot| snapshot.observe(&state.installation, &current, rules))
                {
                    Ok(view) => CandidateObservationV1 {
                        candidate_index: candidate.candidate_index,
                        current_profile_revision: Some(revision.clone()),
                        rate: view.rate,
                        observed_shared_count: view.observed_shared_count,
                        health: view.health,
                        availability: view.availability,
                    },
                    Err(_) => CandidateObservationV1 {
                        candidate_index: candidate.candidate_index,
                        current_profile_revision: Some(revision.clone()),
                        rate: None,
                        observed_shared_count: None,
                        health: HealthObservation::Unavailable,
                        availability: AvailabilityObservation::CapacityDenied,
                    },
                }
            })
            .collect();
        drop(operational);
        manifest.runtime_observation = Some(RuntimeObservationV1 {
            instance_id: self.instance.clone(),
            observed_at,
            candidates,
        });
        manifest.validate().map_err(|_| ErrorCode::CapacityExceeded)
    }
}
