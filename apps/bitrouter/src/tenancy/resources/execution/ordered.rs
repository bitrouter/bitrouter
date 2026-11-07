//! A request-local ordered subset of one retained protocol root.
//!
//! Public ordinals address the persisted root sequence, never the deduplicated
//! private candidate vector. This view narrows membership without mutating the
//! cached revision or granting authority over any resource.

use super::*;
use bitrouter_ai::tenancy::{
    admission::AdmissionMode,
    authority::ResourceKind,
    configuration::LearningMode,
    ordered::{
        AcceptedSelectionV2, CandidateDescriptorV1, CandidateManifestV1, GenerationProtocol,
    },
    validation::Validate,
};
use std::collections::HashSet;

pub(super) struct Selection {
    pub(super) candidates: Vec<usize>,
    pub(super) dependencies: Vec<ResourceRef>,
    ordinals: Vec<u16>,
}

impl RetainedExecutionRevision {
    fn ordered_base(
        &self,
        reference: &ResourceRef,
        inbound: &ApiProtocol,
    ) -> std::result::Result<RetainedRoute, ErrorCode> {
        if self.document.policy.is_some()
            || self.document.learning.mode != LearningMode::Off
            || self.document.learning.max_evaluations != 0
            || !matches!(
                inbound,
                ApiProtocol::ChatCompletions | ApiProtocol::Messages | ApiProtocol::Responses
            )
        {
            return Err(ErrorCode::CapabilityMismatch);
        }
        let route = self.selected_data(reference.resource_id.as_str(), inbound)?;
        if route.reference() != reference {
            return Err(ErrorCode::CapabilityMismatch);
        }
        route.ordered_root()?;
        Ok(route)
    }

    fn selected_ordered_data(
        &self,
        reference: &ResourceRef,
        inbound: &ApiProtocol,
        ordinals: &[u16],
    ) -> std::result::Result<RetainedRoute, ErrorCode> {
        let mut route = self.ordered_base(reference, inbound)?;
        route.narrow(ordinals)?;
        Ok(route)
    }

    pub(in crate::tenancy) fn candidate_manifest(
        &self,
        reference: &ResourceRef,
        inbound_protocol: GenerationProtocol,
        authority_revision: AuthorityRevision,
    ) -> std::result::Result<CandidateManifestV1, ErrorCode> {
        let inbound = match inbound_protocol {
            GenerationProtocol::ChatCompletions => ApiProtocol::ChatCompletions,
            GenerationProtocol::Messages => ApiProtocol::Messages,
            GenerationProtocol::Responses => ApiProtocol::Responses,
        };
        let route = self.ordered_base(reference, &inbound)?;
        let candidates = route
            .ordered_root()?
            .candidates
            .iter()
            .enumerate()
            .map(|(ordinal, index)| {
                let candidate = route
                    .data
                    .candidates
                    .get(*index)
                    .ok_or(ErrorCode::AuthorityUnavailable)?;
                let target = crate::tenancy::receipts::journal::project_target(
                    &self.installation,
                    reference,
                    &candidate.credential,
                    &candidate.target.provider_name,
                    &candidate.target.service_id,
                    candidate.target.account_label.as_deref(),
                )
                .map_err(|_| ErrorCode::CapabilityMismatch)?;
                Ok(CandidateDescriptorV1 {
                    admission_profile: route
                        .profile_for_ordinal(
                            u16::try_from(ordinal).map_err(|_| ErrorCode::CapacityExceeded)?,
                        )?
                        .cloned(),
                    candidate_index: u16::try_from(ordinal)
                        .map_err(|_| ErrorCode::CapacityExceeded)?,
                    target,
                    credential: candidate.credential.clone(),
                })
            })
            .collect::<std::result::Result<Vec<_>, ErrorCode>>()?;
        let manifest = CandidateManifestV1 {
            schema_version: 1,
            admission_mode: route.admission_mode(),
            runtime_observation: None,
            installation_id: self.installation.clone(),
            tenant_id: self.tenant.clone(),
            execution_revision: self.revision.clone(),
            authority_revision,
            route: reference.clone(),
            inbound_protocol,
            candidates,
        };
        manifest
            .validate()
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        Ok(manifest)
    }

    #[cfg(test)]
    pub(in crate::tenancy) fn select_ordered_for_test(
        &self,
        reference: &ResourceRef,
        inbound: &ApiProtocol,
        ordinals: &[u16],
    ) -> std::result::Result<Arc<RetainedRoute>, ErrorCode> {
        self.selected_ordered_data(reference, inbound, ordinals)
            .map(Arc::new)
    }
}

impl CachePin<RetainedExecutionRevision> {
    pub(crate) fn select_ordered(
        &self,
        reference: &ResourceRef,
        inbound: &ApiProtocol,
        ordinals: &[u16],
        mode: AdmissionMode,
    ) -> std::result::Result<Arc<RetainedRoute>, ErrorCode> {
        let mut route = self.selected_ordered_data(reference, inbound, ordinals)?;
        if route.admission_mode() != mode {
            return Err(ErrorCode::CapabilityMismatch);
        }
        route._revision = Some(self.clone());
        Ok(Arc::new(route))
    }
}

impl RetainedRoute {
    pub(crate) fn admission_mode(&self) -> AdmissionMode {
        if self.data.admission.is_some() {
            AdmissionMode::ProfiledV1
        } else {
            AdmissionMode::Legacy
        }
    }

    fn profile_for_ordinal(
        &self,
        ordinal: u16,
    ) -> std::result::Result<Option<&ResourceRef>, ErrorCode> {
        let Some(attachments) = &self.data.admission else {
            return Ok(None);
        };
        let RouteSelection::Wire(inbound) = &self.selection else {
            return Err(ErrorCode::CapabilityMismatch);
        };
        let protocol = generation_protocol(inbound).map_err(|_| ErrorCode::CapabilityMismatch)?;
        attachments
            .iter()
            .find(|item| item.inbound_protocol == protocol && item.candidate_index == ordinal)
            .map(|item| Some(&item.profile))
            .ok_or(ErrorCode::AuthorityUnavailable)
    }

    fn ordered_root(&self) -> std::result::Result<&Chain, ErrorCode> {
        if self.ordered.is_some()
            || self.data.policy.is_some()
            || self.data.assessment.is_some()
            || self.data.block.is_some()
            || self.data.choice.is_some()
        {
            return Err(ErrorCode::CapabilityMismatch);
        }
        if [
            "previous_response_id",
            "conversation",
            "context_management",
            "cachedContent",
        ]
        .iter()
        .any(|field| {
            self.data
                .overrides
                .params
                .get(*field)
                .is_some_and(|value| !value.is_null())
        }) {
            return Err(ErrorCode::CapabilityMismatch);
        }
        let RouteSelection::Wire(inbound) = &self.selection else {
            return Err(ErrorCode::CapabilityMismatch);
        };
        let root = self
            .data
            .chains
            .iter()
            .find(|chain| chain.selector == self.data.selector && chain.inbound == *inbound)
            .ok_or(ErrorCode::CapabilityMismatch)?;
        if root.candidates.len() > 256 {
            return Err(ErrorCode::CapacityExceeded);
        }
        // A dependency not explained by this static root is mandatory and may
        // not be removed merely because the caller supplied a shorter list.
        if self.data.dependencies.iter().any(|reference| {
            reference != self.reference()
                && !self.data.admission.as_ref().is_some_and(|attachments| {
                    attachments.iter().any(|item| &item.profile == reference)
                })
                && (reference.resource_kind != ResourceKind::Credential
                    || !self
                        .data
                        .candidates
                        .iter()
                        .any(|candidate| candidate.credential == *reference))
        }) {
            return Err(ErrorCode::CapabilityMismatch);
        }
        Ok(root)
    }

    fn narrow(&mut self, ordinals: &[u16]) -> std::result::Result<(), ErrorCode> {
        if !(1..=64).contains(&ordinals.len())
            || ordinals.iter().collect::<HashSet<_>>().len() != ordinals.len()
        {
            return Err(ErrorCode::InvalidRequest);
        }
        let root = self.ordered_root()?;
        let mut candidates = Vec::with_capacity(ordinals.len());
        let mut dependencies = vec![self.reference().clone()];
        for ordinal in ordinals {
            let index = *root
                .candidates
                .get(usize::from(*ordinal))
                .ok_or(ErrorCode::InvalidRequest)?;
            if self.admission_mode() == AdmissionMode::Legacy && candidates.contains(&index) {
                return Err(ErrorCode::InvalidRequest);
            }
            let candidate = self
                .data
                .candidates
                .get(index)
                .ok_or(ErrorCode::AuthorityUnavailable)?;
            if candidate
                .target
                .model_constraints
                .input_token_counting
                .is_some()
                || candidate.target.api_key_override.is_some()
                || candidate.target.api_base_override.is_some()
                || candidate
                    .target
                    .headers
                    .iter()
                    .any(|header| header.passthrough())
            {
                return Err(ErrorCode::CapabilityMismatch);
            }
            candidates.push(index);
            if !dependencies.contains(&candidate.credential) {
                dependencies.push(candidate.credential.clone());
            }
            if let Some(profile) = self.profile_for_ordinal(*ordinal)?
                && !dependencies.contains(profile)
            {
                dependencies.push(profile.clone());
            }
        }
        self.ordered = Some(Selection {
            candidates,
            dependencies,
            ordinals: ordinals.to_vec(),
        });
        Ok(())
    }

    pub(crate) fn is_ordered(&self) -> bool {
        self.ordered.is_some()
    }

    /// Bind a private execution cursor to its exact retained root entry. Equal
    /// target projections at different ordinals remain distinct entries.
    pub(in crate::tenancy) fn reached_profile(
        &self,
        rank: usize,
        target: &RoutingTarget,
    ) -> std::result::Result<(u16, &ResourceRef), ErrorCode> {
        let selection = self.ordered.as_ref().ok_or(ErrorCode::CapabilityMismatch)?;
        let ordinal = *selection.ordinals.get(rank).ok_or(ErrorCode::Forbidden)?;
        let index = *selection.candidates.get(rank).ok_or(ErrorCode::Forbidden)?;
        let candidate = self
            .data
            .candidates
            .get(index)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        if candidate.projection != self.candidate_key(target)? {
            return Err(ErrorCode::Forbidden);
        }
        let profile = self
            .profile_for_ordinal(ordinal)?
            .ok_or(ErrorCode::CapabilityMismatch)?;
        Ok((ordinal, profile))
    }

    pub(crate) fn ordered_ordinals(&self) -> Option<&[u16]> {
        self.ordered
            .as_ref()
            .map(|selection| selection.ordinals.as_slice())
    }

    pub(crate) fn accepted_selection(
        &self,
        generation_defaults_digest: Option<&bitrouter_ai::tenancy::identity::Digest>,
    ) -> std::result::Result<Option<AcceptedSelectionV2>, ErrorCode> {
        let Some(selection) = &self.ordered else {
            return Ok(None);
        };
        let inbound_protocol = match &self.selection {
            RouteSelection::Wire(ApiProtocol::ChatCompletions) => {
                GenerationProtocol::ChatCompletions
            }
            RouteSelection::Wire(ApiProtocol::Messages) => GenerationProtocol::Messages,
            RouteSelection::Wire(ApiProtocol::Responses) => GenerationProtocol::Responses,
            _ => return Err(ErrorCode::CapabilityMismatch),
        };
        Ok(Some(AcceptedSelectionV2 {
            generation_defaults_digest: generation_defaults_digest.cloned(),
            admission_mode: self.admission_mode(),
            route: self.reference().clone(),
            inbound_protocol,
            candidate_indices: selection.ordinals.clone(),
        }))
    }

    pub(crate) fn candidate_ordinal(
        &self,
        target: &RoutingTarget,
    ) -> std::result::Result<Option<u16>, ErrorCode> {
        let Some(selection) = &self.ordered else {
            return Ok(None);
        };
        let candidate = self.candidate(target)?;
        selection
            .candidates
            .iter()
            .zip(&selection.ordinals)
            .find(|(index, _)| {
                self.data
                    .candidates
                    .get(**index)
                    .is_some_and(|retained| retained.projection == candidate.projection)
            })
            .map(|(_, ordinal)| Some(*ordinal))
            .ok_or(ErrorCode::Forbidden)
    }
}
