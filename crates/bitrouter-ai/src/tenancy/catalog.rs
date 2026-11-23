//! Read-only configured catalog identity. These descriptors grant no authority.

use serde::{Deserialize, Serialize};

/// Reserved ordered-dispatch body member, removed before provider decoding.
pub const BODY_MEMBER: &str = "bitrouter_catalog_selection";
/// Maximum serialized request-local catalog instructions.
pub const MAX_SELECTION_BYTES: usize = 64 * 1024;

use super::{
    configuration::ResourceRef,
    identity::{ResourceId, RevisionId},
    ordered::{CandidateManifest, GenerationProtocol},
    validation::{Validate, ValidationError, bytes, nullable, require, text},
};

/// A configured target template, including targets without a platform credential.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CatalogTemplateV1 {
    /// Immutable opaque target identity; clients never derive or enumerate it.
    pub template_id: RevisionId,
    /// Stable source association declared by the operator.
    pub source_id: String,
    /// Configured provider identity.
    pub provider: String,
    /// Original nullable configured account label.
    #[serde(deserialize_with = "nullable")]
    pub account: Option<String>,
    /// Actual upstream model.
    pub model: String,
    /// Actual outbound codec.
    pub outbound_protocol: crate::types::ApiProtocol,
    /// Available platform slot, or null for a BYOK-only template.
    #[serde(deserialize_with = "nullable")]
    pub credential_ref: Option<ResourceRef>,
    /// Mandatory admission rules for this operator-funded target.
    pub platform_profile: ResourceRef,
    /// Neutral admission rules for an explicitly selected tenant-owned key.
    pub byok_profile: ResourceRef,
}

/// One entry of a single ordered platform/BYOK execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CatalogCandidateV1 {
    /// Exact configured template from the descriptor.
    pub template_id: RevisionId,
    /// Null selects its platform slot; otherwise an existing tenant-owned slot.
    #[serde(deserialize_with = "nullable")]
    pub credential_ref: Option<ResourceRef>,
    /// Existing immutable tenant route supplying an approved custom BYOK base.
    /// Only an explicit credential owned by the same tenant may accompany it.
    /// Endpoint borrowing requires a codec-supported ordinary owned route.
    /// Private adapter endpoints remain operator configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_ref: Option<ResourceRef>,
}

/// Stateless selection preparation and the identical dispatch instruction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CatalogSelectionRequestV1 {
    /// Selection contract, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    pub schema_version: u8,
    /// Shared file snapshot from catalog readback.
    pub catalog_revision: RevisionId,
    /// Exact configured public root.
    pub route_id: ResourceId,
    /// Inbound protocol for all candidates.
    pub inbound_protocol: GenerationProtocol,
    /// Final order; an owned-key entry and platform entry may use the same template.
    pub candidates: Vec<CatalogCandidateV1>,
}

/// Prepared request-owned immutable root; no management state is created.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CatalogSelectionV1 {
    /// Exact closed instructions to accompany dispatch.
    pub selection: CatalogSelectionRequestV1,
    /// Actual final ordinal/target/credential/profile and source associations.
    pub candidates: CatalogCandidatesV1,
}

impl Validate for CatalogSelectionRequestV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 1, "catalog selection version")?;
        require(
            (1..=super::ordered::MAX_ATTEMPTS).contains(&self.candidates.len()),
            "catalog selection count",
        )?;
        for candidate in &self.candidates {
            if let Some(route) = &candidate.route_ref {
                use super::authority::{OwnerKind, ResourceKind};
                let credential = candidate
                    .credential_ref
                    .as_ref()
                    .ok_or(ValidationError("owned route requires BYOK credential"))?;
                require(
                    route.owner_kind == OwnerKind::Tenant
                        && credential.owner_kind == OwnerKind::Tenant
                        && route.owner_id == credential.owner_id
                        && route.resource_kind == ResourceKind::Route
                        && credential.resource_kind == ResourceKind::Credential,
                    "owned route/credential tenant and kinds",
                )?;
            }
        }
        bytes(self, MAX_SELECTION_BYTES)
    }
}

#[cfg(test)]
mod owned_route_tests {
    use super::super::authority::{OwnerKind, ResourceKind};
    use super::*;
    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn reference(kind: ResourceKind) -> TestResult<ResourceRef> {
        Ok(ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: "tenant".try_into()?,
            resource_kind: kind,
            resource_id: "resource".try_into()?,
            revision_id: "v1".try_into()?,
        })
    }
    fn request() -> TestResult<CatalogSelectionRequestV1> {
        Ok(CatalogSelectionRequestV1 {
            schema_version: 1,
            catalog_revision: "catalog".try_into()?,
            route_id: "route".try_into()?,
            inbound_protocol: GenerationProtocol::ChatCompletions,
            candidates: vec![CatalogCandidateV1 {
                template_id: "template".try_into()?,
                credential_ref: Some(reference(ResourceKind::Credential)?),
                route_ref: None,
            }],
        })
    }
    #[test]
    fn optional_owned_route_is_backward_compatible_and_bound_into_instruction() -> TestResult {
        let original = request()?;
        let bytes = serde_json::to_vec(&original)?;
        assert!(!String::from_utf8_lossy(&bytes).contains("route_ref"));
        assert_eq!(
            serde_json::from_slice::<CatalogSelectionRequestV1>(&bytes)?,
            original
        );
        let mut custom = original.clone();
        custom.candidates[0].route_ref = Some(reference(ResourceKind::Route)?);
        custom.validate()?;
        assert_ne!(serde_json::to_vec(&custom)?, bytes);
        assert_eq!(
            serde_json::from_slice::<CatalogSelectionRequestV1>(&serde_json::to_vec(&custom)?)?,
            custom
        );
        Ok(())
    }
    #[test]
    fn platform_cross_tenant_and_wrong_kind_route_references_fail() -> TestResult {
        let mut request = request()?;
        request.candidates[0].route_ref = Some(reference(ResourceKind::Route)?);
        request.candidates[0].credential_ref = None;
        assert!(request.validate().is_err());
        request.candidates[0].credential_ref = Some(reference(ResourceKind::Credential)?);
        request.candidates[0]
            .route_ref
            .as_mut()
            .ok_or("route absent")?
            .owner_id = "other".try_into()?;
        assert!(request.validate().is_err());
        request.candidates[0].route_ref = Some(reference(ResourceKind::Credential)?);
        assert!(request.validate().is_err());
        request.candidates[0].route_ref = Some(reference(ResourceKind::Route)?);
        request.candidates[0]
            .credential_ref
            .as_mut()
            .ok_or("credential absent")?
            .owner_kind = OwnerKind::OperatorPool;
        assert!(request.validate().is_err());
        Ok(())
    }
}

impl Validate for CatalogSelectionV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        self.selection.validate()?;
        self.candidates.validate()?;
        require(
            self.selection.route_id == self.candidates.manifest.route.resource_id
                && self.selection.inbound_protocol == self.candidates.manifest.inbound_protocol,
            "prepared catalog root binding",
        )?;
        require(
            self.selection.candidates.len() == self.candidates.manifest.candidates.len(),
            "prepared catalog candidate count",
        )
    }
}

/// Explicit source identity for one immutable protocol-root ordinal.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CandidateSourceV1 {
    /// Ordinal in the accompanying manifest, never an inferred position.
    #[serde(deserialize_with = "super::number::integer")]
    pub candidate_index: u16,
    /// Stable deployment-owned source identity, explicitly mapped by the client.
    pub source_id: String,
    /// Configured provider identity before receipt-safe opaque projection.
    pub provider: String,
    /// Exact configured account association; absence is never inferred as funding.
    #[serde(deserialize_with = "nullable")]
    pub account: Option<String>,
    /// Canonical configured upstream model identity.
    pub model: String,
    /// Actual outbound codec, which can differ from the inbound protocol.
    pub outbound_protocol: crate::types::ApiProtocol,
}

/// One authorized configured root and its explicit logical source mapping.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CatalogCandidatesV1 {
    /// Descriptor version, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    pub schema_version: u8,
    /// Existing immutable root, candidate, profile and receipt identities.
    pub manifest: CandidateManifest,
    /// Total mapping in manifest ordinal order.
    pub sources: Vec<CandidateSourceV1>,
    /// Configured templates, including explicitly unsendable platform entries.
    pub templates: Vec<CatalogTemplateV1>,
}

impl Validate for CatalogCandidatesV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 1, "catalog descriptor version")?;
        self.manifest.validate()?;
        require(
            self.manifest.route.owner_kind == super::authority::OwnerKind::OperatorPool
                && self.manifest.admission_mode == super::admission::AdmissionMode::Profiled,
            "configured catalog root",
        )?;
        require(
            self.templates.len() <= super::ordered::MAX_CANDIDATES,
            "catalog template count",
        )?;
        let mut templates = std::collections::HashSet::new();
        for template in &self.templates {
            require(
                templates.insert(&template.template_id),
                "duplicate catalog template",
            )?;
            text(&template.source_id, 512)?;
            text(&template.provider, 128)?;
            text(&template.model, 512)?;
            if let Some(account) = &template.account {
                text(account, 128)?;
            }
            for profile in [&template.platform_profile, &template.byok_profile] {
                super::admission::validate_profile_reference(profile)?;
                require(
                    profile.owner_id == self.manifest.route.owner_id,
                    "catalog profile owner",
                )?;
            }
            if let Some(credential) = &template.credential_ref {
                require(
                    credential.owner_kind == super::authority::OwnerKind::OperatorPool
                        && credential.owner_id == self.manifest.route.owner_id
                        && credential.resource_kind == super::authority::ResourceKind::Credential,
                    "catalog platform credential",
                )?;
            }
        }
        require(
            self.sources.len() == self.manifest.candidates.len(),
            "catalog source count",
        )?;
        for (source, candidate) in self.sources.iter().zip(&self.manifest.candidates) {
            require(
                source.candidate_index == candidate.candidate_index,
                "catalog source ordinal",
            )?;
            text(&source.provider, 128)?;
            text(&source.source_id, 512)?;
            if let Some(account) = &source.account {
                text(account, 128)?;
            }
            text(&source.model, 512)?;
        }
        bytes(self, super::ordered::MAX_MANIFEST_BYTES)
    }
}
