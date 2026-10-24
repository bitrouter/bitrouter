//! Explicit tenant administration and execution-fact lookup over protected HTTP.
//!
//! No local configuration, credential discovery, automatic retry or redirect is
//! performed. Discovery is fresh on every call; its descriptor must be bound to
//! the subsequent submission. The server still checks live authority at dispatch.
//! Use HTTPS or a tunnel to a loopback control listener for remote administration.

mod transport;

use std::time::Duration;

use bitrouter_ai::tenancy::{
    authority::{
        AuditReaderCreate, BindingPut, CredentialCreate, CredentialIssued, CredentialStatus,
        PrincipalPut, TenantPut,
    },
    configuration::{Activation, ActiveRevision, ConfigurationPut, ConfigurationRevision},
    execution::{ExecutionPage, ExecutionReceipt},
    identity::{
        BindingId, CorrelationId, CredentialId, ExecutionId, InstallationId, OperationId,
        PrincipalId, RevisionId, TenantId,
    },
    operation::{Mutation, OperationResult, TenantCapabilities},
};
use reqwest::Method;

use crate::{
    error::{Error, Result},
    gateway::HttpVersion,
};

/// Bounded management transport, independent of inference deadlines.
#[derive(Clone, Debug)]
pub struct TenancyOptions {
    /// HTTP/1.1, negotiated HTTP/2, or explicit h2c prior knowledge.
    pub http_version: HttpVersion,
    /// Explicit opt-in to environment/system proxy discovery.
    pub system_proxy: bool,
    /// Positive connection deadline, at most five minutes; default three seconds.
    pub connect_timeout: Duration,
    /// Positive whole-request deadline, at most five minutes; default five seconds.
    pub request_timeout: Duration,
    /// Maximum decoded response bytes, at most 16 MiB; default 2 MiB.
    pub max_response_bytes: usize,
}
impl Default for TenancyOptions {
    fn default() -> Self {
        Self {
            http_version: HttpVersion::default(),
            system_proxy: false,
            connect_timeout: Duration::from_secs(3),
            request_timeout: Duration::from_secs(5),
            max_response_bytes: 2 * 1024 * 1024,
        }
    }
}

/// One protected control endpoint with one explicit ingress credential.
#[derive(Clone)]
pub struct TenancyClient {
    endpoint: url::Url,
    http: reqwest::Client,
    options: TenancyOptions,
}
impl std::fmt::Debug for TenancyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenancyClient")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl TenancyClient {
    /// Construct an explicit client with bounded defaults.
    ///
    /// # Errors
    /// Rejects unsafe endpoints, invalid credentials or HTTP client setup failure.
    pub fn new(endpoint: &str, credential: &str) -> Result<Self> {
        Self::with_options(endpoint, credential, TenancyOptions::default())
    }

    /// Construct an explicit client; credentials are never forwarded by redirects.
    ///
    /// # Errors
    /// Rejects invalid endpoint/credential/bounds or transport setup failure.
    pub fn with_options(endpoint: &str, credential: &str, options: TenancyOptions) -> Result<Self> {
        transport::build(endpoint, credential, options)
    }

    /// Fetch a fresh descriptor; the caller explicitly decides whether to submit.
    ///
    /// # Errors
    /// Returns authentication, version, transport or bounded response errors.
    pub async fn capabilities(&self) -> Result<TenantCapabilities> {
        self.get("tenancy/capabilities", None).await
    }

    /// Create a tenant or change lifecycle with installation-operator authority.
    ///
    /// # Errors
    /// Returns validation/authorization/CAS errors or an unknown command outcome.
    pub async fn put_tenant(
        &self,
        tenant: &TenantId,
        input: &TenantPut,
    ) -> Result<OperationResult> {
        let result: OperationResult = self
            .mutate(
                Method::PUT,
                &format!("tenants/{tenant}"),
                &input.installation_id,
                &input.operation_id,
                input,
            )
            .await?;
        transport::operation_result(result, &input.operation_id, Some(tenant.as_str()), None)
    }

    /// Provision an immutable issuer/subject identity as an installation operator.
    ///
    /// # Errors
    /// Returns validation/authorization/CAS errors or an unknown command outcome.
    pub async fn put_principal(
        &self,
        principal: &PrincipalId,
        input: &PrincipalPut,
    ) -> Result<OperationResult> {
        let result = self
            .mutate(
                Method::PUT,
                &format!("principals/{principal}"),
                &input.installation_id,
                &input.operation_id,
                input,
            )
            .await?;
        transport::operation_result(result, &input.operation_id, Some(principal.as_str()), None)
    }

    /// Create/update a scoped managed binding; current server grant ceilings apply.
    ///
    /// # Errors
    /// Returns validation/authorization/managed-writer/CAS errors or an unknown outcome.
    pub async fn put_binding(
        &self,
        tenant: &TenantId,
        binding: &BindingId,
        input: &BindingPut,
    ) -> Result<OperationResult> {
        let result = self
            .mutate(
                Method::PUT,
                &format!("tenants/{tenant}/bindings/{binding}"),
                &input.installation_id,
                &input.operation_id,
                input,
            )
            .await?;
        transport::operation_result(result, &input.operation_id, Some(binding.as_str()), None)
    }

    /// Issue or atomically rotate a credential. A replay never recovers the secret.
    ///
    /// # Errors
    /// Returns invalid delegation, CAS or service errors; lost responses are unknown.
    pub async fn issue_credential(
        &self,
        tenant: &TenantId,
        input: &CredentialCreate,
    ) -> Result<CredentialIssued> {
        let result = self
            .mutate(
                Method::POST,
                &format!("tenants/{tenant}/credentials"),
                &input.installation_id,
                &input.operation_id,
                input,
            )
            .await?;
        transport::issued(result, tenant, &input.principal_id, &input.operation_id)
    }

    /// Read the current credential revision and lifecycle; never its secret.
    ///
    /// # Errors
    /// Returns current-authority, not-found, transport or response-binding errors.
    pub async fn credential(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        credential: &CredentialId,
    ) -> Result<CredentialStatus> {
        let result: CredentialStatus = self
            .get(
                &format!("tenants/{tenant}/credentials/{credential}"),
                Some(installation),
            )
            .await?;
        transport::response_matches(
            result.tenant_id == *tenant && result.credential_id == *credential,
        )?;
        Ok(result)
    }

    /// Revoke only the observed credential revision; no implicit latest lookup.
    ///
    /// # Errors
    /// Returns authority/CAS errors or an unknown command outcome requiring recovery.
    pub async fn revoke_credential(
        &self,
        tenant: &TenantId,
        credential: &CredentialId,
        input: &Mutation,
    ) -> Result<OperationResult> {
        let result = self
            .mutate(
                Method::DELETE,
                &format!("tenants/{tenant}/credentials/{credential}"),
                &input.installation_id,
                &input.operation_id,
                input,
            )
            .await?;
        transport::operation_result(result, &input.operation_id, Some(credential.as_str()), None)
    }

    /// Provision a receipt-only audit reader with installation-operator authority.
    ///
    /// # Errors
    /// Returns authorization/CAS errors or an unknown issuance outcome.
    pub async fn issue_audit_reader(
        &self,
        tenant: &TenantId,
        input: &AuditReaderCreate,
    ) -> Result<CredentialIssued> {
        let result = self
            .mutate(
                Method::POST,
                &format!("tenants/{tenant}/audit-readers"),
                &input.installation_id,
                &input.operation_id,
                input,
            )
            .await?;
        transport::issued(result, tenant, &input.principal_id, &input.operation_id)
    }

    /// Detach a managed writer as an installation operator; ordinary writers cannot reclaim it.
    ///
    /// # Errors
    /// Returns authorization/CAS errors or an unknown command outcome.
    pub async fn detach_binding_management(
        &self,
        tenant: &TenantId,
        binding: &BindingId,
        input: &Mutation,
    ) -> Result<OperationResult> {
        let result = self
            .mutate(
                Method::DELETE,
                &format!("tenants/{tenant}/bindings/{binding}/management"),
                &input.installation_id,
                &input.operation_id,
                input,
            )
            .await?;
        transport::operation_result(result, &input.operation_id, Some(binding.as_str()), None)
    }

    /// Submit a bounded typed draft; this does not activate it.
    ///
    /// # Errors
    /// Rejects invalid local documents before HTTP; server authority/compile errors
    /// and unknown command outcomes remain explicit.
    pub async fn put_configuration(
        &self,
        tenant: &TenantId,
        input: &ConfigurationPut,
    ) -> Result<ConfigurationRevision> {
        let result: ConfigurationRevision = self
            .mutate(
                Method::POST,
                &format!("tenants/{tenant}/configurations"),
                &input.installation_id,
                &input.operation_id,
                input,
            )
            .await?;
        transport::response_matches(
            result.tenant_id == *tenant && result.document == input.document,
        )
        .map_err(|e| transport::unknown(&input.operation_id, e))?;
        Ok(result)
    }

    /// Read an exact retained revision under current resource grants.
    ///
    /// # Errors
    /// Returns current-authority, not-found or transport/response-binding errors.
    pub async fn configuration(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        revision: &RevisionId,
    ) -> Result<ConfigurationRevision> {
        let result: ConfigurationRevision = self
            .get(
                &format!("tenants/{tenant}/configurations/{revision}"),
                Some(installation),
            )
            .await?;
        transport::response_matches(
            result.tenant_id == *tenant && result.revision_id == *revision,
        )?;
        Ok(result)
    }

    /// Activate or roll back an immutable revision with whole-tenant scope and CAS.
    ///
    /// # Errors
    /// Returns authority/reference/CAS errors or an unknown command outcome.
    pub async fn activate(&self, tenant: &TenantId, input: &Activation) -> Result<OperationResult> {
        let result = self
            .mutate(
                Method::POST,
                &format!("tenants/{tenant}/activation"),
                &input.installation_id,
                &input.operation_id,
                input,
            )
            .await?;
        transport::operation_result(result, &input.operation_id, None, Some(&input.revision_id))
    }

    /// Read the active revision without substituting it into an existing submission.
    ///
    /// # Errors
    /// Returns current-authority or bounded transport/response errors.
    pub async fn active_revision(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
    ) -> Result<ActiveRevision> {
        let result: ActiveRevision = self
            .get(&format!("tenants/{tenant}/activation"), Some(installation))
            .await?;
        transport::response_matches(result.tenant_id == *tenant)?;
        Ok(result)
    }

    /// Recover the caller's nonsecret management result using current read authority.
    ///
    /// # Errors
    /// Returns forbidden/not-found/expired or bounded transport/response errors.
    pub async fn operation(
        &self,
        installation: &InstallationId,
        operation: &OperationId,
    ) -> Result<OperationResult> {
        let result: OperationResult = self
            .get(
                &format!("tenancy/operations/{operation}"),
                Some(installation),
            )
            .await?;
        transport::response_matches(result.operation_id == *operation)?;
        Ok(result)
    }

    /// Read owner-bound execution facts; never retries or re-executes inference.
    ///
    /// # Errors
    /// Returns current receipt authority, not-found or transport/response-binding errors.
    pub async fn execution(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        execution: &ExecutionId,
    ) -> Result<ExecutionReceipt> {
        let result: ExecutionReceipt = self
            .get(
                &format!("tenants/{tenant}/executions/{execution}"),
                Some(installation),
            )
            .await?;
        transport::response_matches(
            result.installation_id == *installation
                && result.tenant_id == *tenant
                && result.execution_id == *execution,
        )?;
        Ok(result)
    }

    /// Query a correlation page; multiple matching executions are a normal success.
    ///
    /// # Errors
    /// Rejects limits outside 1..=100 before HTTP; returns authority or response errors.
    pub async fn executions(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        correlation: &CorrelationId,
        after: Option<&ExecutionId>,
        limit: u16,
    ) -> Result<ExecutionPage> {
        if !(1..=100).contains(&limit) {
            return Err(Error::Request("execution page limit must be 1..=100"));
        }
        let mut url = self.url(&format!("tenants/{tenant}/executions"))?;
        url.query_pairs_mut()
            .append_pair("correlation_id", correlation.as_str())
            .append_pair("limit", &limit.to_string());
        if let Some(after) = after {
            url.query_pairs_mut().append_pair("after", after.as_str());
        }
        let result: ExecutionPage = self
            .send(
                self.http
                    .get(url)
                    .header("Bitrouter-Installation-Id", installation.as_str()),
            )
            .await?;
        transport::response_matches(
            result.installation_id == *installation
                && result.correlation_id == *correlation
                && result.execution_ids.len() <= usize::from(limit)
                && after.is_none_or(|cursor| result.execution_ids.iter().all(|id| id > cursor)),
        )?;
        Ok(result)
    }
}
