//! Explicit shared-gateway administration, independent of inference and control contexts.

use std::time::Duration;

use bitrouter_ai::gateway::{execution::GatewayExecution, operator::*};
use reqwest::{Response, header};
use serde::de::DeserializeOwned;
use url::{Host, Url};

use crate::{
    error::{Error, Result},
    gateway::{HttpVersion, TransportOptions},
    transport,
};

const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;

/// Bounded operator transport. No mutation or read is retried automatically.
#[derive(Clone, Debug)]
pub struct OperatorOptions {
    /// HTTP version policy; defaults to [`HttpVersion::Auto`].
    pub http_version: HttpVersion,
    /// Opt into environment/system proxy discovery; disabled by default.
    pub system_proxy: bool,
    /// Positive connection deadline, at most 300 seconds; default five seconds.
    pub connect_timeout: Duration,
    /// Positive whole-request deadline, at most 300 seconds; default 15 seconds.
    pub request_timeout: Duration,
    /// Accepted response size, 1 byte through 16 MiB; default 2 MiB.
    pub max_response_bytes: usize,
}

impl Default for OperatorOptions {
    fn default() -> Self {
        Self {
            http_version: HttpVersion::default(),
            system_proxy: false,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(15),
            max_response_bytes: 2 * 1024 * 1024,
        }
    }
}

/// One explicitly addressed operator endpoint and credential. No discovery or fallback.
#[derive(Clone)]
pub struct OperatorClient {
    endpoint: Url,
    http: reqwest::Client,
    options: OperatorOptions,
}

impl std::fmt::Debug for OperatorClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperatorClient")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl OperatorClient {
    /// Create a client with bounded defaults and no endpoint discovery.
    ///
    /// # Errors
    /// Rejects invalid endpoint/credential syntax or HTTP client setup failures.
    pub fn new(endpoint: &str, token: &str) -> Result<Self> {
        Self::with_options(endpoint, token, OperatorOptions::default())
    }

    /// Create a client with explicit deadlines and response bounds.
    ///
    /// # Errors
    /// Rejects unsafe endpoints, invalid credentials/bounds or HTTP setup failures.
    pub fn with_options(endpoint: &str, token: &str, options: OperatorOptions) -> Result<Self> {
        let endpoint = endpoint_url(endpoint)?;
        if !valid_token(token) {
            return Err(Error::Options(
                "operator token must contain 32..4096 Bearer-compatible bytes",
            ));
        }
        if options.connect_timeout.is_zero()
            || options.request_timeout.is_zero()
            || options.connect_timeout > Duration::from_secs(300)
            || options.request_timeout > Duration::from_secs(300)
            || !(1..=16 * 1024 * 1024).contains(&options.max_response_bytes)
        {
            return Err(Error::Options(
                "operator deadlines and response bound must be positive and bounded",
            ));
        }
        let mut headers = header::HeaderMap::new();
        let mut credential = header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Error::Options("invalid operator credential"))?;
        credential.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, credential);
        let transport_options = TransportOptions {
            http_version: options.http_version,
            system_proxy: options.system_proxy,
            connect_timeout: options.connect_timeout,
            read_timeout: Some(options.request_timeout),
            request_timeout: Some(options.request_timeout),
            max_response_bytes: options.max_response_bytes,
            ..TransportOptions::default()
        };
        // The shared transport disables redirects and reqwest protocol retries.
        let http = transport::build_operator_http(headers, &transport_options)?;
        Ok(Self {
            endpoint,
            http,
            options,
        })
    }

    /// Submit once. An uncertain response retains only the original operation ID.
    /// Cancellation also leaves the outcome unknown; retain the caller's input privately.
    ///
    /// # Errors
    /// Invalid local input fails before dispatch. Validated definite rejections
    /// return [`Error::OperatorRejected`]; other post-dispatch failures return
    /// [`Error::OperatorUnknown`] and require receipt recovery, not a new ID.
    pub async fn apply(&self, mutation: &Mutation) -> Result<Receipt> {
        validate_handle(&mutation.operation_id)?;
        let bytes = serde_json::to_vec(mutation)
            .map_err(|_| Error::Request("invalid operator mutation"))?;
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(Error::Request("operator mutation exceeds 2 MiB"));
        }
        let url = self.url("mutations")?;
        let response = self
            .http
            .post(url)
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|error| unknown(&mutation.operation_id, Error::transport(error)))?;
        let status = response.status().as_u16();
        let bytes = self
            .response_bytes(response)
            .await
            .map_err(|error| unknown(&mutation.operation_id, error))?;
        if status != 200 {
            let error = service_error(status, &bytes, Some(&mutation.operation_id));
            return match error {
                Error::OperatorRejected(
                    code @ (ErrorCode::Unauthorized
                    | ErrorCode::Forbidden
                    | ErrorCode::InvalidInput
                    | ErrorCode::Conflict
                    | ErrorCode::NotFound),
                ) => Err(Error::OperatorRejected(code)),
                error => Err(unknown(&mutation.operation_id, error)),
            };
        }
        let receipt: Receipt =
            decode(&bytes).map_err(|error| unknown(&mutation.operation_id, error))?;
        if receipt.operation_id != mutation.operation_id
            || !outcome_matches(&mutation.command, &receipt.outcome)
        {
            return Err(unknown(
                &mutation.operation_id,
                Error::Response("operator receipt does not match the mutation"),
            ));
        }
        Ok(receipt)
    }

    /// `Unresolved` is absence of a visible receipt, not proof of rollback.
    ///
    /// # Errors
    /// Fails for invalid IDs, denied access, transport/size errors or malformed
    /// responses. A failed read does not resolve an earlier uncertain mutation.
    pub async fn operation(&self, operation_id: &str) -> Result<OperationLookup> {
        validate_handle(operation_id)?;
        let result: OperationLookup = self
            .get("operations", &[("operation_id", operation_id)])
            .await?;
        let actual = match &result {
            OperationLookup::Committed { receipt } => &receipt.operation_id,
            OperationLookup::Unresolved { operation_id } => operation_id,
        };
        if actual != operation_id {
            return Err(Error::Response("operator lookup identity mismatch"));
        }
        Ok(result)
    }

    /// Read the active configuration/key generations.
    ///
    /// # Errors
    /// Fails on denied access, transport/size errors or malformed responses.
    pub async fn revision(&self) -> Result<Revision> {
        self.get("revision", &[]).await
    }

    /// Read a tenant's current policy without changing its accounting.
    ///
    /// # Errors
    /// Fails for invalid IDs, denied/missing records, transport/size errors or
    /// malformed responses, including a mismatched returned identity.
    pub async fn tenant(&self, id: &str) -> Result<TenantPolicy> {
        validate_id(id)?;
        let result: TenantPolicy = self.get("tenants", &[("id", id)]).await?;
        if result.tenant_id != id {
            return Err(Error::Response("operator tenant identity mismatch"));
        }
        Ok(result)
    }

    /// Read a resource pool's current policy.
    ///
    /// # Errors
    /// Fails for invalid IDs, denied/missing records, transport/size errors or
    /// malformed responses, including a mismatched returned identity.
    pub async fn resource(&self, id: &str) -> Result<ResourcePolicy> {
        validate_id(id)?;
        let result: ResourcePolicy = self.get("resources", &[("id", id)]).await?;
        if result.resource_pool_id != id {
            return Err(Error::Response("operator resource identity mismatch"));
        }
        Ok(result)
    }

    /// Read principal metadata; requires the database-wide principal grant.
    ///
    /// # Errors
    /// Fails for invalid IDs, denied/missing records, transport/size errors or
    /// malformed responses, including a mismatched returned identity.
    pub async fn principal(&self, id: &str) -> Result<PrincipalMetadata> {
        validate_id(id)?;
        let result: PrincipalMetadata = self.get("principals", &[("id", id)]).await?;
        if result.principal_id != id {
            return Err(Error::Response("operator principal identity mismatch"));
        }
        Ok(result)
    }

    /// Read credential version/binding metadata, never stored secret material.
    ///
    /// # Errors
    /// Fails for invalid identities, denied/missing records, transport/size errors
    /// or malformed responses, including a mismatched returned account tuple.
    pub async fn credential(&self, provider: &str, account: &str) -> Result<CredentialMetadata> {
        validate_id(provider)?;
        validate_id(account)?;
        let result: CredentialMetadata = self
            .get(
                "credentials",
                &[("provider", provider), ("account", account)],
            )
            .await?;
        if result.provider != provider || result.account != account {
            return Err(Error::Response("operator credential identity mismatch"));
        }
        Ok(result)
    }

    /// Read an execution's durable status without polling or repeating inference.
    ///
    /// # Errors
    /// Fails for invalid IDs, denied/missing records, transport/size errors or
    /// malformed responses, including a mismatched returned identity.
    pub async fn execution(&self, id: &str) -> Result<GatewayExecution> {
        validate_id(id)?;
        let result: GatewayExecution = self.get("executions", &[("id", id)]).await?;
        if result.execution_id != id {
            return Err(Error::Response("operator execution identity mismatch"));
        }
        Ok(result)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let response = self
            .http
            .get(self.url(path)?)
            .query(query)
            .send()
            .await
            .map_err(Error::transport)?;
        let status = response.status().as_u16();
        let bytes = self.response_bytes(response).await?;
        if status != 200 {
            return Err(service_error(status, &bytes, None));
        }
        decode(&bytes)
    }

    async fn response_bytes(&self, response: Response) -> Result<Vec<u8>> {
        let mut types = response.headers().get_all(header::CONTENT_TYPE).iter();
        let valid_type = types
            .next()
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                crate::gateway::execution::media_type::matches(value, "application/json")
            });
        if !valid_type
            || types.next().is_some()
            || response.headers().contains_key(header::CONTENT_ENCODING)
        {
            return Err(Error::Response("operator response is not JSON"));
        }
        transport::body(response, self.options.max_response_bytes).await
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.endpoint
            .join(path)
            .map_err(|_| Error::Options("invalid operator endpoint"))
    }
}

fn valid_token(token: &str) -> bool {
    // https://www.rfc-editor.org/rfc/rfc6750.html#section-2.1
    let value = token.trim_end_matches('=');
    (32..=4096).contains(&token.len())
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~+/".contains(&byte))
}

fn validate_handle(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(Error::Request("invalid operator operation ID"));
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        return Err(Error::Request("invalid operator resource ID"));
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|_| Error::Response("invalid operator response"))
}

fn service_error(status: u16, bytes: &[u8], operation_id: Option<&str>) -> Error {
    let Ok(body) = serde_json::from_slice::<ErrorResponse>(bytes) else {
        return Error::Response("invalid operator error response");
    };
    if let (Some(expected), Some(actual)) = (operation_id, body.operation_id.as_deref())
        && expected != actual
    {
        return Error::Response("operator error identity mismatch");
    }
    if matches!(
        (status, body.code),
        (400, ErrorCode::InvalidInput)
            | (401, ErrorCode::Unauthorized)
            | (403, ErrorCode::Forbidden)
            | (404, ErrorCode::NotFound)
            | (409, ErrorCode::Conflict)
            | (503, ErrorCode::Unavailable | ErrorCode::Unknown)
    ) {
        Error::OperatorRejected(body.code)
    } else {
        Error::Response("operator error status mismatch")
    }
}

fn unknown(operation_id: &str, source: Error) -> Error {
    Error::OperatorUnknown {
        operation_id: operation_id.into(),
        source: Box::new(source),
    }
}

fn outcome_matches(command: &Command, outcome: &Outcome) -> bool {
    match (command, outcome) {
        (Command::PutTenant(input), Outcome::Tenant(output)) => input.tenant_id == output.tenant_id,
        (Command::PutResource(input), Outcome::Resource(output)) => {
            input.resource_pool_id == output.resource_pool_id
        }
        (Command::PutPrincipal(input), Outcome::Principal(output)) => {
            input.principal_id == output.principal_id && input.tenant_id == output.tenant_id
        }
        (Command::ImportCredential(input), Outcome::Credential(output)) => {
            input.provider == output.provider
                && input.account == output.account
                && input.resource_pool_id == output.resource_pool_id
        }
        (Command::Publish(input), Outcome::Published(output)) => {
            input.expected_generation.checked_add(1) == Some(output.config_generation)
        }
        (
            Command::RotateCorrelation {
                expected_generation,
            },
            Outcome::CorrelationRotated(output),
        ) => expected_generation.checked_add(1) == Some(output.key_generation),
        (Command::ResolveExecution { execution_id, .. }, Outcome::Execution(output)) => {
            execution_id == &output.execution_id
        }
        _ => false,
    }
}

fn endpoint_url(endpoint: &str) -> Result<Url> {
    let mut url = Url::parse(endpoint).map_err(|_| Error::Options("invalid operator endpoint"))?;
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Options(
            "operator endpoint cannot contain userinfo, query or fragment",
        ));
    }
    let loopback = match url.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(host)) => host.is_loopback(),
        Some(Host::Ipv6(host)) => host.is_loopback(),
        None => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(Error::Options(
            "operator endpoint requires HTTPS or loopback HTTP",
        ));
    }
    match url.path().trim_end_matches('/') {
        "" => url.set_path("/operator/v1/"),
        path if path.ends_with("/operator/v1") => {
            let path = format!("{path}/");
            url.set_path(&path);
        }
        _ => {
            return Err(Error::Options(
                "operator endpoint path must end in /operator/v1",
            ));
        }
    }
    Ok(url)
}
