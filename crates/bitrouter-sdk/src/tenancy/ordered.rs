//! Explicit discovery and bounded readback for ordered dispatch.

use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    identity::{InstallationId, RevisionId, TenantId},
    ordered::{CandidateManifest, GenerationProtocol, MAX_MANIFEST_BYTES, OrderedCapabilities},
};

use super::{TenancyClient, transport};
use crate::error::{Error, Result};

impl TenancyClient {
    /// Discover ordered v2 support independently of frozen v1 capabilities.
    /// No installation is assumed before discovery; the returned identity binds later calls.
    ///
    /// # Errors
    /// Returns authentication, unsupported version, bounded transport or contract errors.
    pub async fn ordered_capabilities(&self) -> Result<OrderedCapabilities> {
        self.get("tenancy/ordered/capabilities", None).await
    }

    /// Discover private ordered GenerateContent support for the current process.
    ///
    /// # Errors
    /// Returns authentication, unsupported endpoint, bounded transport or contract errors.
    pub async fn ordered_generate_content_capabilities(
        &self,
    ) -> Result<bitrouter_ai::tenancy::generate_content::OrderedGenerateContentCapabilitiesV1> {
        self.get("tenancy/ordered/generate-content/capabilities", None)
            .await
    }

    /// Read an operator-only nonsecret manifest of one exact retained protocol root.
    /// The result confers no dispatch authority and never expands a caller selection.
    ///
    /// # Errors
    /// Rejects a non-route reference locally; returns authority, bounds or binding errors.
    pub async fn candidate_manifest(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        revision: &RevisionId,
        route: &ResourceRef,
        protocol: GenerationProtocol,
    ) -> Result<CandidateManifest> {
        if route.resource_kind != ResourceKind::Route {
            return Err(Error::Request(
                "candidate manifest requires a route reference",
            ));
        }
        let mut url = self.url(&format!(
            "tenancy/tenants/{tenant}/configurations/{revision}/candidates"
        ))?;
        let owner_kind = match route.owner_kind {
            OwnerKind::Tenant => "tenant",
            OwnerKind::OperatorPool => "operator_pool",
        };
        let inbound_protocol = match protocol {
            GenerationProtocol::ChatCompletions => "chat_completions",
            GenerationProtocol::Messages => "messages",
            GenerationProtocol::Responses => "responses",
            GenerationProtocol::GenerateContent => "generate_content",
        };
        url.query_pairs_mut()
            .append_pair("route_owner_kind", owner_kind)
            .append_pair("route_owner_id", route.owner_id.as_str())
            .append_pair("route_id", route.resource_id.as_str())
            .append_pair("route_revision", route.revision_id.as_str())
            .append_pair("inbound_protocol", inbound_protocol);
        let result: CandidateManifest = self
            .send_bounded(
                self.http
                    .get(url)
                    .header("Bitrouter-Installation-Id", installation.as_str()),
                MAX_MANIFEST_BYTES,
            )
            .await?;
        transport::response_matches(
            result.installation_id == *installation
                && result.tenant_id == *tenant
                && result.execution_revision == *revision
                && result.route == *route
                && result.inbound_protocol == protocol,
        )?;
        Ok(result)
    }
}
