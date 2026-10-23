use reqwest::{
    Response,
    header::{CONTENT_TYPE, HeaderMap},
};
use serde::de::DeserializeOwned;
use url::Url;

use super::{
    ExecutionMetadata, GatewayClient, media_type,
    options::{normalized_identity, valid_identity},
};
use crate::{
    error::{Error, Result, ServiceFailure},
    transport,
};

pub(super) struct Evidence {
    status: u16,
    headers: HeaderMap,
}

impl Evidence {
    pub fn new(response: &Response) -> Self {
        Self {
            status: response.status().as_u16(),
            headers: response.headers().clone(),
        }
    }

    pub fn failure(self, error: Error) -> Error {
        Error::Service(Box::new(ServiceFailure {
            status: self.status,
            headers: self.headers,
            body: None,
            body_error: Some(error),
        }))
    }
}

fn one<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .ok_or(Error::Response("missing shared response metadata"))?;
    if values.next().is_some() {
        return Err(Error::Response("duplicate shared response metadata"));
    }
    value
        .to_str()
        .map_err(|_| Error::Response("shared response metadata is not textual"))
}

pub(super) fn content_type(headers: &HeaderMap, expected: &str) -> Result<()> {
    if !media_type::matches(one(headers, CONTENT_TYPE.as_str())?, expected) {
        return Err(Error::Response("unexpected shared response content type"));
    }
    Ok(())
}

pub(super) async fn json<T: DeserializeOwned>(response: Response, limit: usize) -> Result<T> {
    content_type(response.headers(), "application/json")?;
    serde_json::from_slice(&transport::body(response, limit).await?)
        .map_err(|_| Error::Response("body does not match the shared JSON contract"))
}

pub(super) fn resource(client: &GatewayClient, id: &str) -> Result<Url> {
    if !valid_identity(id) || matches!(id, "." | "..") {
        return Err(Error::Request(
            "execution ID must be a bounded nonempty path segment",
        ));
    }
    let mut url = client.url("v1/gateway/executions")?;
    // push encodes an opaque ID as one segment, including slash and percent.
    // https://docs.rs/url/latest/url/struct.PathSegmentsMut.html#method.push
    url.path_segments_mut()
        .map_err(|_| Error::Options("service endpoint cannot contain path segments"))?
        .push(id);
    Ok(url)
}

pub(super) fn metadata(
    client: &GatewayClient,
    headers: &HeaderMap,
    expected_correlation: Option<&str>,
) -> Result<ExecutionMetadata> {
    let execution = one(headers, "x-bitrouter-execution-id")?;
    let correlation = one(headers, "x-bitrouter-request-id")?;
    if !normalized_identity(execution)
        || !normalized_identity(correlation)
        || expected_correlation.is_some_and(|expected| expected != correlation)
    {
        return Err(Error::Response("invalid shared response identity"));
    }
    let canonical =
        resource(client, execution).map_err(|_| Error::Response("invalid shared execution ID"))?;
    let mut root = client.endpoint.clone();
    root.set_path("/v1/gateway/executions");
    root.path_segments_mut()
        .map_err(|_| Error::Options("service endpoint cannot contain path segments"))?
        .push(execution);
    let location = one(headers, "location")?;
    // Location is evidence only. Credential routing always uses `resource`.
    // https://www.rfc-editor.org/rfc/rfc9110.html#section-10.2.2
    let valid = location == root.path()
        || location == canonical.path()
        || Url::parse(location).is_ok_and(|url| {
            url.origin() == canonical.origin()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && (url.path() == root.path() || url.path() == canonical.path())
        });
    if !valid {
        return Err(Error::Response("invalid shared status location"));
    }
    Ok(ExecutionMetadata {
        execution_id: execution.into(),
        correlation_id: correlation.into(),
        status_location: canonical.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn location_is_evidence_never_a_new_origin_or_proxy_prefix() -> Result<()> {
        let client = GatewayClient::new("https://gateway.example/proxy/v1", None)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bitrouter-execution-id",
            HeaderValue::from_static("execution-1"),
        );
        headers.insert(
            "x-bitrouter-request-id",
            HeaderValue::from_static("correlation"),
        );
        for location in [
            "/v1/gateway/executions/execution-1",
            "/proxy/v1/gateway/executions/execution-1",
            "https://gateway.example/v1/gateway/executions/execution-1",
            "https://gateway.example/proxy/v1/gateway/executions/execution-1",
        ] {
            headers.insert("location", HeaderValue::from_static(location));
            assert_eq!(
                metadata(&client, &headers, None)?.status_location,
                "https://gateway.example/proxy/v1/gateway/executions/execution-1"
            );
        }
        for location in [
            "//gateway.example/v1/gateway/executions/execution-1",
            "https://other.example/v1/gateway/executions/execution-1",
            "http://gateway.example/v1/gateway/executions/execution-1",
            "https://gateway.example:8443/v1/gateway/executions/execution-1",
            "https://user@gateway.example/v1/gateway/executions/execution-1",
            "/v1/gateway/executions/other-id",
            "/other/v1/gateway/executions/execution-1",
            "/v1/gateway/executions/execution-1/extra",
            "/v1/gateway/executions/execution-1?replay=1",
            "https://gateway.example/v1/gateway/executions/execution-1#fragment",
        ] {
            headers.insert("location", HeaderValue::from_static(location));
            assert!(metadata(&client, &headers, None).is_err());
        }
        Ok(())
    }
}
