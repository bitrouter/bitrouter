use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;

use crate::gateway::error::BitrouterError;

pub(super) const CORRELATION_HEADER: &str = "x-bitrouter-request-id";
pub(super) const EXECUTION_HEADER: &str = "x-bitrouter-execution-id";
const EXECUTIONS_PATH: &str = "/v1/gateway/executions/";

pub(super) fn request_identity(
    headers: &HeaderMap,
) -> Result<(String, String, Option<String>), BitrouterError> {
    let key_hash = credential_hash(headers)?;
    let correlation = single_identity(headers, CORRELATION_HEADER)?
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let idempotency = single_identity(headers, "idempotency-key")?;
    Ok((key_hash, correlation, idempotency))
}

pub(super) fn credential_hash(headers: &HeaderMap) -> Result<String, BitrouterError> {
    if headers.get_all(header::AUTHORIZATION).iter().count() > 1
        || headers.get_all("x-api-key").iter().count() > 1
    {
        return Err(BitrouterError::bad_request(
            "duplicate authentication header",
        ));
    }
    crate::auth::hook::credential_from_headers(headers)
        .map(|key| crate::auth::keys::hash_key(&key))
        .ok_or_else(|| BitrouterError::Unauthorized("shared authentication required".into()))
}

fn single_identity(headers: &HeaderMap, name: &str) -> Result<Option<String>, BitrouterError> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(BitrouterError::bad_request(
            "duplicate shared request identity",
        ));
    }
    let value = value
        .to_str()
        .map_err(|_| BitrouterError::bad_request("shared request identity is not text"))?
        .trim();
    if !valid_identity(value) {
        return Err(BitrouterError::bad_request(
            "shared request identity must contain 1..256 bytes without control characters",
        ));
    }
    Ok(Some(value.into()))
}

pub(super) fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

pub(super) struct Identity {
    pub correlation: HeaderValue,
    execution: HeaderValue,
    location: HeaderValue,
}

impl Identity {
    pub fn new(correlation: &str, execution: &str) -> Result<Self, BitrouterError> {
        let invalid = |_| BitrouterError::internal("invalid shared response identity");
        Ok(Self {
            correlation: HeaderValue::from_str(correlation).map_err(invalid)?,
            execution: HeaderValue::from_str(execution).map_err(invalid)?,
            location: HeaderValue::from_str(&format!("{EXECUTIONS_PATH}{execution}"))
                .map_err(invalid)?,
        })
    }
    pub fn attach(self, mut response: Response) -> Response {
        response
            .headers_mut()
            .insert(CORRELATION_HEADER, self.correlation);
        response
            .headers_mut()
            .insert(EXECUTION_HEADER, self.execution);
        // Location names a status resource, not a redirect/replay instruction.
        // https://www.rfc-editor.org/rfc/rfc9110.html#section-10.2.2
        response
            .headers_mut()
            .insert(header::LOCATION, self.location);
        response
    }
}
