//! Read-only profile queries use bounded transport without mutation outcomes.

use bitrouter_ai::tenancy::{
    identity::InstallationId,
    profile_observation::{
        AdmissionProfileObservationCapabilitiesV1, AdmissionProfileObservationRequestV1,
        AdmissionProfileObservationSnapshotV1, MAX_BYTES,
    },
    validation::Validate,
};

use super::{TenancyClient, transport};
use crate::error::{Error, Result};

impl TenancyClient {
    /// Discover the operator-only exact-profile reader without an assumed installation.
    /// This feature is independent of route manifests and grants no dispatch authority.
    ///
    /// # Errors
    /// Returns authentication, unsupported feature, bounded transport or contract errors.
    pub async fn admission_profile_observation_capabilities(
        &self,
    ) -> Result<AdmissionProfileObservationCapabilitiesV1> {
        self.send_bounded(
            self.http
                .get(self.url("tenancy/admission-profiles/observations/capabilities")?),
            MAX_BYTES,
        )
        .await
    }

    /// Observe exact retained profiles in request order at one authority revision.
    /// No retry, batching, route discovery, reservation or mutation is performed.
    /// Callers also check the actual instance and their freshness/deadline policy.
    ///
    /// # Errors
    /// Rejects invalid queries locally; returns bounded read, authority or binding errors.
    pub async fn observe_admission_profiles(
        &self,
        installation: &InstallationId,
        request: &AdmissionProfileObservationRequestV1,
    ) -> Result<AdmissionProfileObservationSnapshotV1> {
        request
            .validate()
            .map_err(|_| Error::Request("invalid profile observation query"))?;
        let body = serde_json::to_vec(request)
            .map_err(|_| Error::Request("invalid profile observation encoding"))?;
        if body.len() > MAX_BYTES {
            return Err(Error::Request(
                "profile observation query exceeds byte limit",
            ));
        }
        let result: AdmissionProfileObservationSnapshotV1 = self
            .send_bounded(
                self.http
                    .post(self.url("tenancy/admission-profiles/observations/v1")?)
                    .header("Bitrouter-Installation-Id", installation.as_str())
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body),
                MAX_BYTES,
            )
            .await?;
        transport::response_matches(
            result.installation_id == *installation
                && result.authority_revision == request.expected_authority_revision
                && result.profiles.len() == request.profiles.len()
                && result
                    .profiles
                    .iter()
                    .zip(&request.profiles)
                    .all(|(observed, requested)| observed.profile == *requested),
        )?;
        Ok(result)
    }
}
