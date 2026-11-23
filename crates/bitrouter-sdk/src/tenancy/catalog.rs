//! Read-only configured roots; observation never grants dispatch authority.

use bitrouter_ai::tenancy::{
    catalog::{CatalogCandidatesV1, CatalogSelectionRequestV1, CatalogSelectionV1},
    identity::{InstallationId, ResourceId, TenantId},
    ordered::GenerationProtocol,
};

use super::{Error, Result, TenancyClient};

impl TenancyClient {
    /// Resolve one final configured/BYOK chain without publishing configuration.
    /// The same instructions must accompany its subsequent ordered dispatch.
    ///
    /// # Errors
    /// Returns invalid selection, authorization, transport or response-binding errors.
    pub async fn prepare_catalog_selection(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        selection: &CatalogSelectionRequestV1,
    ) -> Result<CatalogSelectionV1> {
        use bitrouter_ai::tenancy::validation::Validate;
        selection
            .validate()
            .map_err(|_| Error::Request("invalid configured selection"))?;
        let result: CatalogSelectionV1 = self
            .send(
                self.http
                    .post(self.url(&format!("tenants/{tenant}/catalog/selections"))?)
                    .header("Bitrouter-Installation-Id", installation.as_str())
                    .json(selection),
            )
            .await?;
        super::transport::response_matches(
            result.selection == *selection
                && result.candidates.manifest.installation_id == *installation
                && result.candidates.manifest.tenant_id == *tenant,
        )?;
        Ok(result)
    }

    /// Read one configured root under the caller's current exact tenant grants.
    ///
    /// # Errors
    /// Returns authorization, transport or invalid descriptor errors.
    pub async fn catalog_candidates(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        route: &str,
        inbound_protocol: GenerationProtocol,
    ) -> Result<CatalogCandidatesV1> {
        ResourceId::try_from(route)
            .map_err(|_| Error::Request("invalid configured route identity"))?;
        let path = format!("tenants/{tenant}/catalog/routes/{route}/candidates");
        let mut url = self.url(&path)?;
        url.query_pairs_mut().append_pair(
            "inbound_protocol",
            match inbound_protocol {
                GenerationProtocol::ChatCompletions => "chat_completions",
                GenerationProtocol::Messages => "messages",
                GenerationProtocol::Responses => "responses",
                GenerationProtocol::GenerateContent => "generate_content",
            },
        );
        let result: CatalogCandidatesV1 = self
            .send(
                self.http
                    .get(url)
                    .header("Bitrouter-Installation-Id", installation.as_str()),
            )
            .await?;
        super::transport::response_matches(
            result.manifest.installation_id == *installation
                && result.manifest.tenant_id == *tenant
                && result.manifest.route.resource_id.as_str() == route
                && result.manifest.inbound_protocol == inbound_protocol,
        )?;
        Ok(result)
    }
}
