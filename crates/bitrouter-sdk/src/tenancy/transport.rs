use bitrouter_ai::tenancy::{
    authority::CredentialIssued,
    identity::{InstallationId, OperationId, PrincipalId, RevisionId, TenantId},
    operation::{
        DispatchState, Error as TenancyError, ErrorCode, OperationResult, OperationStatus,
    },
    validation::Validate,
};
use reqwest::{Method, RequestBuilder, header};
use serde::{Serialize, de::DeserializeOwned};

use super::{TenancyClient, TenancyOptions};
use crate::{
    error::{Error, Result, ServiceFailure},
    gateway::TransportOptions,
};

pub(super) fn build(
    endpoint: &str,
    credential: &str,
    options: TenancyOptions,
) -> Result<TenancyClient> {
    let endpoint = url::Url::parse(&crate::control::normalize_endpoint(endpoint)?)
        .map_err(|_| Error::Options("invalid tenancy endpoint"))?;
    // Bearer credentials use RFC 6750's b64token alphabet, without whitespace.
    // https://www.rfc-editor.org/rfc/rfc6750#section-2.1
    let bare = credential.trim_end_matches('=');
    if !(1..=4096).contains(&credential.len())
        || bare.is_empty()
        || !bare
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/".contains(&b))
    {
        return Err(Error::Options(
            "tenancy credential must be 1..4096 Bearer-compatible bytes",
        ));
    }
    let max_timeout = std::time::Duration::from_secs(300);
    if options.connect_timeout.is_zero()
        || options.request_timeout.is_zero()
        || options.connect_timeout > max_timeout
        || options.request_timeout > max_timeout
        || !(1..=16 * 1024 * 1024).contains(&options.max_response_bytes)
    {
        return Err(Error::Options(
            "tenancy deadlines and response limit must be positive and bounded",
        ));
    }
    let mut headers = header::HeaderMap::new();
    let mut token = header::HeaderValue::from_str(&format!("Bearer {credential}"))
        .map_err(|_| Error::Options("invalid tenancy credential"))?;
    token.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, token);
    headers.insert(
        header::ACCEPT,
        header::HeaderValue::from_static("application/json"),
    );
    let policy = TransportOptions {
        http_version: options.http_version,
        system_proxy: options.system_proxy,
        connect_timeout: options.connect_timeout,
        read_timeout: Some(options.request_timeout),
        request_timeout: Some(options.request_timeout),
        max_response_bytes: options.max_response_bytes,
        ..TransportOptions::default()
    };
    let http = crate::transport::build_operator_http(headers, &policy)?;
    Ok(TenancyClient {
        endpoint,
        http,
        options,
    })
}

impl TenancyClient {
    pub(super) fn url(&self, path: &str) -> Result<url::Url> {
        self.endpoint
            .join(path)
            .map_err(|_| Error::Request("invalid tenancy path"))
    }

    pub(super) async fn get<T: DeserializeOwned + Validate>(
        &self,
        path: &str,
        installation: Option<&InstallationId>,
    ) -> Result<T> {
        let mut request = self.http.get(self.url(path)?);
        if let Some(installation) = installation {
            request = request.header("Bitrouter-Installation-Id", installation.as_str());
        }
        self.send(request).await
    }

    pub(super) async fn mutate<I: Serialize + Validate, O: DeserializeOwned + Validate>(
        &self,
        method: Method,
        path: &str,
        installation: &InstallationId,
        operation: &OperationId,
        input: &I,
    ) -> Result<O> {
        input
            .validate()
            .map_err(|_| Error::Request("invalid tenancy command contract"))?;
        let body = serde_json::to_vec(input)
            .map_err(|_| Error::Request("invalid tenancy command encoding"))?;
        if body.len() > 2 * 1024 * 1024 {
            return Err(Error::Request("tenancy command exceeds 2 MiB"));
        }
        let request = self
            .http
            .request(method, self.url(path)?)
            .header("Bitrouter-Installation-Id", installation.as_str())
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        match self.send(request).await {
            Ok(result) => Ok(result),
            Err(error) if definite_rejection(&error, operation) => Err(error),
            Err(error) => Err(unknown(operation, error)),
        }
    }

    pub(super) async fn send<T: DeserializeOwned + Validate>(
        &self,
        request: RequestBuilder,
    ) -> Result<T> {
        self.send_bounded(request, self.options.max_response_bytes)
            .await
    }

    pub(super) async fn send_bounded<T: DeserializeOwned + Validate>(
        &self,
        request: RequestBuilder,
        contract_limit: usize,
    ) -> Result<T> {
        let response = request.send().await.map_err(Error::transport)?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let failure = |body, body_error| {
            Error::Tenancy(Box::new(ServiceFailure {
                status,
                headers: headers.clone(),
                body,
                body_error,
            }))
        };
        if headers.get_all(header::CONTENT_TYPE).iter().count() != 1
            || !crate::transport::has_content_type(&response, "application/json")
            || headers
                .get_all(header::CONTENT_ENCODING)
                .iter()
                .any(|value| value != "identity")
        {
            return Err(failure(
                None,
                Some(Error::Response("tenancy response must be unencoded JSON")),
            ));
        }
        let bytes = crate::transport::body(
            response,
            contract_limit.min(self.options.max_response_bytes),
        )
        .await
        .map_err(|error| failure(None, Some(error)))?;
        if !(200..300).contains(&status) {
            let result: Result<TenancyError> = decode(&bytes);
            return match result {
                Ok(body) => Err(failure(Some(body), None)),
                Err(error) => Err(failure(None, Some(error))),
            };
        }
        decode(&bytes).map_err(|error| failure(None, Some(error)))
    }
}

fn decode<T: DeserializeOwned + Validate>(bytes: &[u8]) -> Result<T> {
    // Decode directly into strict typed structs, preserving duplicate-field errors.
    // Do not round-trip through Value, which would silently discard duplicates.
    let result: T = serde_json::from_slice(bytes)
        .map_err(|_| Error::Response("invalid tenancy response shape"))?;
    result
        .validate()
        .map_err(|_| Error::Response("invalid tenancy response invariants"))?;
    Ok(result)
}

fn definite_rejection(error: &Error, operation: &OperationId) -> bool {
    let Error::Tenancy(failure) = error else {
        return false;
    };
    let Some(body) = &failure.body else {
        return false;
    };
    if failure.body_error.is_some()
        || body.dispatch_state != DispatchState::NotStarted
        || body.operation_id.as_ref().is_some_and(|id| id != operation)
    {
        return false;
    }
    matches!(
        (failure.status, body.code),
        (400, ErrorCode::InvalidRequest)
            | (401, ErrorCode::Unauthorized)
            | (403, ErrorCode::Forbidden | ErrorCode::TenantInactive)
            | (404, ErrorCode::NotFound)
            | (
                409,
                ErrorCode::RevisionConflict
                    | ErrorCode::OperationConflict
                    | ErrorCode::CapabilityMismatch
            )
            | (410, ErrorCode::OperationExpired)
            | (413, ErrorCode::PayloadTooLarge)
            | (429, ErrorCode::CapacityExceeded)
    )
}

pub(super) fn unknown(operation: &OperationId, source: Error) -> Error {
    Error::TenancyUnknown {
        operation_id: operation.clone(),
        source: Box::new(source),
    }
}

pub(super) fn response_matches(matches: bool) -> Result<()> {
    if matches {
        Ok(())
    } else {
        Err(Error::Response(
            "tenancy response identity differs from request",
        ))
    }
}

pub(super) fn operation_result(
    result: OperationResult,
    operation: &OperationId,
    resource: Option<&str>,
    revision: Option<&RevisionId>,
) -> Result<OperationResult> {
    response_matches(result.operation_id == *operation)
        .map_err(|error| unknown(operation, error))?;
    if result.status == OperationStatus::Applied {
        response_matches(
            resource.is_none_or(|expected| {
                result
                    .resource_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == expected)
            }) && revision
                .is_none_or(|expected| result.resource_revision.as_ref() == Some(expected)),
        )
        .map_err(|error| unknown(operation, error))?;
    }
    Ok(result)
}

pub(super) fn issued(
    result: CredentialIssued,
    tenant: &TenantId,
    principal: &PrincipalId,
    operation: &OperationId,
) -> Result<CredentialIssued> {
    response_matches(
        result.tenant_id == *tenant
            && result.principal_id == *principal
            && result.operation_id == *operation,
    )
    .map_err(|error| unknown(operation, error))?;
    Ok(result)
}
