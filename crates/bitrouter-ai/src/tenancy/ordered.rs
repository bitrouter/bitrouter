//! Revision-bound selection for strict, ordered static-route dispatch.
//!
//! These values grant no authority. The service checks retained membership and
//! current grants; header encoding and transport remain outside this crate.

#[cfg(feature = "schema")]
mod schema;

use serde::{Deserialize, Serialize};

use super::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    execution::Target,
    identity::{
        AuthorityRevision, CorrelationId, InstallationId, InstanceId, RevisionId, TenantId,
    },
    validation::{Validate, ValidationError, bytes, require, unique},
};

/// Maximum selected candidates and configured attempt ceiling.
pub const MAX_ATTEMPTS: usize = 64;
/// Number of persisted ordinals in one protocol root.
pub const MAX_CANDIDATES: usize = 256;
/// Maximum serialized operator candidate manifest.
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
/// Maximum encoded selection header, enforced by the transport boundary.
pub const MAX_SELECTION_HEADER_BYTES: usize = 4096;

/// The three ingress protocols qualified for ordered dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationProtocol {
    /// Chat completions ingress.
    ChatCompletions,
    /// Messages ingress.
    Messages,
    /// Responses ingress.
    Responses,
}

/// Ordered subset of a retained protocol root; indices are stable root ordinals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderedCandidateSelectionV1 {
    /// Selection format, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    pub schema_version: u8,
    /// Exact retained route revision.
    pub route: ResourceRef,
    /// Unique ordinals below 256, in the requested dispatch order, length 1–64.
    #[serde(deserialize_with = "indices")]
    pub candidate_indices: Vec<u16>,
}
impl Validate for OrderedCandidateSelectionV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 1, "ordered selection version")?;
        validate_selection(&self.route, &self.candidate_indices)
    }
}
impl OrderedCandidateSelectionV1 {
    /// Record the ingress protocol observed by the accepting service.
    ///
    /// # Errors
    /// Rejects an invalid request selection. Retained membership and authority
    /// must be checked independently before accepting the request.
    pub fn accept(
        &self,
        inbound_protocol: GenerationProtocol,
    ) -> Result<AcceptedSelectionV2, ValidationError> {
        self.validate()?;
        Ok(AcceptedSelectionV2 {
            route: self.route.clone(),
            inbound_protocol,
            candidate_indices: self.candidate_indices.clone(),
        })
    }
}

/// Accepted selection bound to the actual ingress endpoint, not a caller guess.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedSelectionV2 {
    /// Exact retained route revision.
    pub route: ResourceRef,
    /// Actual ingress protocol, which qualifies the root ordinals.
    pub inbound_protocol: GenerationProtocol,
    /// Accepted order; skipped candidates never become fabricated attempts.
    #[serde(deserialize_with = "indices")]
    pub candidate_indices: Vec<u16>,
}
impl Validate for AcceptedSelectionV2 {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_selection(&self.route, &self.candidate_indices)
    }
}

fn validate_selection(route: &ResourceRef, indices: &[u16]) -> Result<(), ValidationError> {
    require(
        route.resource_kind == ResourceKind::Route,
        "selection route kind",
    )?;
    unique(indices, 1, MAX_ATTEMPTS)?;
    require(
        indices
            .iter()
            .all(|&index| usize::from(index) < MAX_CANDIDATES),
        "candidate ordinal range",
    )
}

pub(super) fn validate_tenant_reference(
    reference: &ResourceRef,
    tenant: &TenantId,
) -> Result<(), ValidationError> {
    // Operator-pool access still requires live authorization at the service.
    require(
        reference.owner_kind != OwnerKind::Tenant || reference.owner_id.as_str() == tenant.as_str(),
        "ordered reference tenant",
    )
}

fn indices<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<u16>, D::Error> {
    #[derive(Deserialize)]
    #[serde(transparent)]
    struct Index(#[serde(deserialize_with = "super::number::integer")] u16);
    Vec::<Index>::deserialize(deserializer).map(|values| values.into_iter().map(|v| v.0).collect())
}

/// Operator-visible identity of a persisted protocol-root entry, without secrets.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateDescriptorV1 {
    /// Persisted root ordinal, not a deduplicated runtime-vector index.
    #[serde(deserialize_with = "super::number::integer")]
    pub candidate_index: u16,
    /// Existing opaque public receipt identity; no endpoint or header defaults.
    pub target: Target,
    /// Exact retained upstream credential reference.
    pub credential: ResourceRef,
}
impl Validate for CandidateDescriptorV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            usize::from(self.candidate_index) < MAX_CANDIDATES,
            "candidate ordinal range",
        )?;
        require(
            self.credential.resource_kind == ResourceKind::Credential,
            "candidate credential kind",
        )?;
        require(
            self.target.credential_version == self.credential.revision_id,
            "candidate credential revision",
        )?;
        self.target.validate()
    }
}

/// Operator-only readback of one retained root. A manifest is not an execution grant.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateManifestV1 {
    /// Manifest format, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    pub schema_version: u8,
    /// Bound authority store.
    pub installation_id: InstallationId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Exact retained configuration.
    pub execution_revision: RevisionId,
    /// Authority snapshot observed during the operator read.
    pub authority_revision: AuthorityRevision,
    /// Exact addressed route.
    pub route: ResourceRef,
    /// Protocol whose persisted root is described.
    pub inbound_protocol: GenerationProtocol,
    /// At most 256 entries in strictly increasing root-ordinal order.
    /// Sparse ordinals retain their original meaning; duplicate targets may occur.
    pub candidates: Vec<CandidateDescriptorV1>,
}
impl Validate for CandidateManifestV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 1, "candidate manifest version")?;
        require(
            self.route.resource_kind == ResourceKind::Route,
            "manifest route kind",
        )?;
        validate_tenant_reference(&self.route, &self.tenant_id)?;
        require(
            self.candidates.len() <= MAX_CANDIDATES,
            "candidate manifest count",
        )?;
        require(
            self.candidates
                .windows(2)
                .all(|pair| pair[0].candidate_index < pair[1].candidate_index),
            "candidate manifest ordinal order",
        )?;
        for candidate in &self.candidates {
            candidate.validate()?;
            validate_tenant_reference(&candidate.credential, &self.tenant_id)?;
        }
        bytes(self, MAX_MANIFEST_BYTES)
    }
}

/// Separate discovery for format two; the version-one capabilities remain frozen.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderedCapabilitiesV2 {
    /// Supported ordered tenancy contract, exactly two.
    #[serde(deserialize_with = "super::number::integer")]
    pub contract_version: u32,
    /// Stable authority store identity.
    pub installation_id: InstallationId,
    /// Current process incarnation.
    pub instance_id: InstanceId,
    /// Dispatch eligibility and mode revision.
    pub capability_revision: RevisionId,
    /// Authority snapshot observed during discovery.
    pub authority_revision: AuthorityRevision,
    /// Configured whole-selection ceiling, 1–64; never a truncation instruction.
    #[serde(deserialize_with = "super::number::integer")]
    pub attempt_limit: u8,
    /// Exact retained execution revisions are enforced.
    pub revision_bound_dispatch: bool,
    /// Durable strict terminal receipts are available.
    pub strict_receipts: bool,
    /// New dispatch is currently enabled under authority and storage fences.
    pub dispatch_enabled: bool,
}
impl Validate for OrderedCapabilitiesV2 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.contract_version == 2, "ordered contract version")?;
        validate_attempt_limit(self.attempt_limit)
    }
}
impl OrderedCapabilitiesV2 {
    /// Bind an explicit selection without downgrade, replay or automatic retry.
    ///
    /// # Errors
    /// Rejects invalid selection, insufficient whole-chain capacity or disabled
    /// strict guarantees. The server must recheck all bindings and live authority.
    pub fn bind(
        &self,
        execution_revision: RevisionId,
        correlation_id: CorrelationId,
        selection: OrderedCandidateSelectionV1,
    ) -> Result<OrderedDispatchBindingV2, ValidationError> {
        self.validate()?;
        selection.validate()?;
        require(
            self.revision_bound_dispatch && self.strict_receipts && self.dispatch_enabled,
            "strict ordered dispatch ineligible",
        )?;
        require(
            selection.candidate_indices.len() <= usize::from(self.attempt_limit),
            "ordered attempt capacity",
        )?;
        Ok(OrderedDispatchBindingV2 {
            contract_version: "2".into(),
            installation_id: self.installation_id.clone(),
            instance_id: self.instance_id.clone(),
            capability_revision: self.capability_revision.clone(),
            execution_revision,
            correlation_id,
            receipt_mode: "strict".into(),
            selection,
        })
    }
}

/// Strict version-two submission metadata with an unencoded typed selection.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderedDispatchBindingV2 {
    /// Tenancy contract, exactly `2`.
    #[serde(rename = "Bitrouter-Tenancy-Version")]
    pub contract_version: String,
    /// Addressed authority store.
    #[serde(rename = "Bitrouter-Installation-Id")]
    pub installation_id: InstallationId,
    /// Addressed process incarnation.
    #[serde(rename = "Bitrouter-Instance-Id")]
    pub instance_id: InstanceId,
    /// Observed capability revision.
    #[serde(rename = "Bitrouter-Capability-Revision")]
    pub capability_revision: RevisionId,
    /// Exact retained execution revision.
    #[serde(rename = "Bitrouter-Execution-Revision")]
    pub execution_revision: RevisionId,
    /// Correlation only, never deduplication.
    #[serde(rename = "Bitrouter-Correlation-Id")]
    pub correlation_id: CorrelationId,
    /// Receipt mode, exactly `strict`.
    #[serde(rename = "Bitrouter-Receipt-Mode")]
    pub receipt_mode: String,
    /// Transport encodes this value in the bounded candidate-selection header.
    pub selection: OrderedCandidateSelectionV1,
}
impl Validate for OrderedDispatchBindingV2 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.contract_version == "2" && self.receipt_mode == "strict",
            "strict ordered dispatch metadata",
        )?;
        self.selection.validate()
    }
}

pub(super) fn validate_attempt_limit(limit: u8) -> Result<(), ValidationError> {
    require(
        (1..=MAX_ATTEMPTS).contains(&usize::from(limit)),
        "ordered attempt limit",
    )
}
