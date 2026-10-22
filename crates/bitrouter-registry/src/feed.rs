//! Credential-isolated, bounded GET feeds. No endpoint or credential discovery.

use registry_json as serde_json;
use std::collections::BTreeSet;
use std::time::Duration;

use http::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Url};
use serde_json::Value;

use crate::document::{MAX_DOCUMENT_BYTES, RegistryError, integer, parse_json};
use crate::mapping::{Observation, normalize, read};

/// A client with mandatory redirect, connection, and request bounds.
pub struct Transport(Client);

/// Exact response bytes and their admitted JSON, for reproducible identity audits.
pub struct Fetched {
    /// Parsed response, with exact numbers and unknown metadata retained.
    pub value: Value,
    /// Original decoded UTF-8 body used for the response digest/audit artifact.
    pub bytes: Vec<u8>,
}

/// Validate a declaration without accessing credentials or the network.
///
/// # Errors
/// Unknown semantics and malformed request/pagination settings are rejected.
pub fn validate(feed: &Value) -> Result<(), RegistryError> {
    crate::mapping::validate(feed)?;
    crate::reconcile::validate_state(feed)?;
    Request::parse(feed, |_| Some("validation-placeholder".into())).map(|_| ())
}

impl Transport {
    /// Construct an isolated transport, optionally trusting private TLS roots.
    /// Callers cannot inject origin credentials, cookies, or redirect behavior.
    ///
    /// # Errors
    /// A TLS/client initialization failure is a normal error.
    pub fn new(
        roots: impl IntoIterator<Item = reqwest::Certificate>,
    ) -> Result<Self, RegistryError> {
        let mut builder = Client::builder();
        for root in roots {
            builder = builder.add_root_certificate(root);
        }
        let client = builder
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| error("registry.network", "Cannot initialize feed HTTP client"))?;
        Ok(Self(client))
    }

    /// Fetch all configured pages before normalizing any inventory observation.
    /// Only the explicitly named environment secret is requested from `secret`.
    ///
    /// # Errors
    /// Configuration, credentials, HTTP, limits, or incomplete interpretation
    /// reject the complete observation. Error text never includes URLs or bodies.
    pub async fn observe(
        &self,
        feed: &Value,
        secret: impl Fn(&str) -> Option<String>,
    ) -> Result<Observation, RegistryError> {
        crate::mapping::validate(feed)?;
        crate::reconcile::validate_state(feed)?;
        let request = Request::parse(feed, secret)?;
        let mut cursor: Option<String> = None;
        let mut seen = BTreeSet::new();
        let mut pages = Vec::new();
        let mut remaining = MAX_DOCUMENT_BYTES;
        let mut remaining_nodes = crate::document::MAX_DOCUMENT_NODES;
        loop {
            let mut url = request.url.clone();
            if let Some(value) = &cursor {
                url.query_pairs_mut().append_pair(&request.parameter, value);
            }
            let fetched = self.get(url, &request.headers, &mut remaining).await?;
            let page = fetched.value;
            remaining_nodes = remaining_nodes
                .checked_sub(crate::mapping::nodes(&page))
                .ok_or_else(|| invalid("Aggregate feed pages exceed value budget"))?;
            let next = request.next(&page)?;
            pages.push(page);
            let Some(next) = next else { break };
            if pages.len() >= request.max_pages || !seen.insert(next.clone()) {
                return Err(error(
                    "registry.incomplete",
                    "Pagination repeats a cursor or exceeds max_pages",
                ));
            }
            cursor = Some(next);
        }
        if feed["kind"] == crate::openrouter::KIND {
            crate::openrouter::observation(
                feed,
                pages
                    .first()
                    .ok_or_else(|| invalid("Missing OpenRouter page"))?,
            )
        } else {
            normalize(feed, &pages)
        }
    }

    /// Fetch one public JSON identity source, with the same strict request rules.
    ///
    /// # Errors
    /// Rejects invalid request declarations and failed or oversized responses.
    pub async fn identity(
        &self,
        request: &Value,
        secret: impl Fn(&str) -> Option<String>,
    ) -> Result<Fetched, RegistryError> {
        let request = Request::parse(
            &serde_json::json!({
                "request": request, "pagination": {"kind":"none"}
            }),
            secret,
        )?;
        let mut remaining = MAX_DOCUMENT_BYTES;
        self.get(request.url, &request.headers, &mut remaining)
            .await
    }

    async fn get(
        &self,
        url: Url,
        headers: &HeaderMap,
        remaining: &mut usize,
    ) -> Result<Fetched, RegistryError> {
        for attempt in 0..3 {
            let response = self
                .0
                .get(url.clone())
                .headers(headers.clone())
                .send()
                .await;
            let mut response = match response {
                Ok(response) => response,
                Err(err) => {
                    if attempt < 2 && (err.is_timeout() || err.is_connect()) {
                        tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
                        continue;
                    }
                    return Err(error(
                        "registry.network",
                        "Feed connection or request failed",
                    ));
                }
            };
            let status = response.status();
            if attempt < 2 && (status.as_u16() == 429 || status.is_server_error()) {
                tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
                continue;
            }
            if status.as_u16() != 200 {
                return Err(error(
                    "registry.http",
                    match status.as_u16() {
                        401 | 403 => "Feed authentication rejected; no anonymous retry",
                        300..=399 => "Feed redirect rejected; update its configured URL",
                        _ => "Feed returned an unsuccessful HTTP status",
                    },
                ));
            }
            if response
                .content_length()
                .is_some_and(|size| size > *remaining as u64)
            {
                return Err(error(
                    "registry.resource_limit",
                    "Aggregate feed responses exceed 32 MiB",
                ));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| error("registry.network", "Feed response body failed"))?
            {
                if chunk.len() > *remaining {
                    return Err(error(
                        "registry.resource_limit",
                        "Aggregate feed responses exceed 32 MiB",
                    ));
                }
                *remaining -= chunk.len();
                bytes.extend_from_slice(&chunk);
            }
            return Ok(Fetched {
                value: parse_json(&bytes)?,
                bytes,
            });
        }
        Err(error("registry.network", "Feed retries exhausted"))
    }
}

struct Request {
    url: Url,
    headers: HeaderMap,
    cursor_path: Option<String>,
    has_more: Option<String>,
    parameter: String,
    max_pages: usize,
}

impl Request {
    fn parse(feed: &Value, secret: impl Fn(&str) -> Option<String>) -> Result<Self, RegistryError> {
        let config = &feed["request"];
        let mut url =
            Url::parse(text(&config["url"])?).map_err(|_| invalid("Invalid absolute feed URL"))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid(
                "Feed URLs must use HTTPS without userinfo or fragments",
            ));
        }
        let mut names = BTreeSet::new();
        for (name, _) in url.query_pairs() {
            if !names.insert(name.into_owned()) {
                return Err(invalid("Duplicate URL query parameter"));
            }
        }
        if let Some(query) = config.get("query") {
            for (name, value) in query
                .as_object()
                .ok_or_else(|| invalid("Feed query must be an object"))?
            {
                if !names.insert(name.clone()) {
                    return Err(invalid("Query parameters collide"));
                }
                url.query_pairs_mut().append_pair(name, text(value)?);
            }
        }
        let mut headers = HeaderMap::new();
        if let Some(constants) = config.get("headers") {
            for (name, value) in constants
                .as_object()
                .ok_or_else(|| invalid("Feed headers must be an object"))?
            {
                insert_header(&mut headers, name, text(value)?)?;
            }
        }
        let auth = &config["auth"];
        match text(&auth["kind"])? {
            "none" => {}
            "bearer" | "header" => {
                let env = text(&auth["env"])?;
                if !env.bytes().enumerate().all(|(i, b)| {
                    b == b'_' || b.is_ascii_uppercase() || (i > 0 && b.is_ascii_digit())
                }) || env.is_empty()
                {
                    return Err(invalid("Invalid secret environment variable name"));
                }
                let key = secret(env).filter(|s| !s.is_empty()).ok_or_else(|| {
                    error("missing_secret", "Required feed secret is missing or empty")
                })?;
                if auth["kind"] == "bearer" {
                    insert_header(&mut headers, "authorization", &format!("Bearer {key}"))?;
                } else {
                    insert_header(&mut headers, text(&auth["header"])?, &key)?;
                }
            }
            _ => {
                return Err(error(
                    "registry.unsupported",
                    "Unsupported feed authentication; update software",
                ));
            }
        }
        let pagination = &feed["pagination"];
        let (cursor_path, has_more, parameter, max_pages) = match text(&pagination["kind"])? {
            "none" => (None, None, String::new(), 1),
            "cursor" => {
                let path = text(&pagination["cursor_path"])?;
                crate::mapping::tokens(path)?;
                let more = pagination.get("has_more_path").map(text).transpose()?;
                if let Some(path) = more {
                    crate::mapping::tokens(path)?;
                }
                let parameter = text(&pagination["parameter"])?;
                if parameter.is_empty() || !names.insert(parameter.into()) {
                    return Err(invalid("Pagination query parameter collides or is empty"));
                }
                let max = pagination
                    .get("max_pages")
                    .map(integer)
                    .unwrap_or(Some(100))
                    .filter(|n| *n > 0 && *n <= 1000)
                    .ok_or_else(|| invalid("max_pages must be between 1 and 1000"))?;
                (
                    Some(path.into()),
                    more.map(Into::into),
                    parameter.into(),
                    max as usize,
                )
            }
            _ => {
                return Err(error(
                    "registry.unsupported",
                    "Unsupported pagination; update software",
                ));
            }
        };
        Ok(Self {
            url,
            headers,
            cursor_path,
            has_more,
            parameter,
            max_pages,
        })
    }

    fn next(&self, page: &Value) -> Result<Option<String>, RegistryError> {
        let Some(path) = &self.cursor_path else {
            return Ok(None);
        };
        if let Some(more) = &self.has_more {
            match read(page, more)?.and_then(Value::as_bool) {
                Some(false) => return Ok(None),
                Some(true) => {}
                None => return Err(invalid("Pagination has_more is missing or not boolean")),
            }
        }
        match read(page, path)? {
            Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
            Some(Value::Null) | Some(Value::String(_)) if self.has_more.is_none() => Ok(None),
            _ => Err(invalid("Pagination cursor is missing or invalid")),
        }
    }
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), RegistryError> {
    let name =
        HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid("Invalid HTTP header name"))?;
    let value = HeaderValue::from_str(value).map_err(|_| invalid("Invalid HTTP header value"))?;
    if headers.insert(name, value).is_some() {
        return Err(invalid("Case-insensitive header collision"));
    }
    Ok(())
}
fn text(value: &Value) -> Result<&str, RegistryError> {
    value
        .as_str()
        .ok_or_else(|| invalid("Expected a string in feed configuration"))
}
fn invalid(message: &str) -> RegistryError {
    error("registry.invalid_data", message)
}
fn error(code: &'static str, message: &str) -> RegistryError {
    RegistryError {
        code,
        path: String::new(),
        message: message.into(),
        recovery: "Repair feed configuration or update software; retain the previous inventory"
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use registry_json::json;

    #[test]
    fn pagination_has_explicit_terminal_states_and_no_host_changes() -> Result<(), RegistryError> {
        let config = json!({"request":{"url":"https://example.com/models","auth":{"kind":"none"}},"pagination":{"kind":"cursor","cursor_path":"/next","parameter":"after"}});
        let request = Request::parse(&config, |_| None)?;
        assert_eq!(request.next(&json!({"next":"A /?"}))?, Some("A /?".into()));
        assert!(request.next(&json!({"next":null}))?.is_none());
        assert!(request.next(&json!({})).is_err());
        let mut config = config;
        config["pagination"]["has_more_path"] = json!("/more");
        let request = Request::parse(&config, |_| None)?;
        assert!(request.next(&json!({"more":false}))?.is_none());
        assert!(request.next(&json!({"more":true})).is_err());
        Ok(())
    }

    #[test]
    fn credentials_and_query_collisions_are_fail_closed_and_sanitized() -> Result<(), RegistryError>
    {
        let mut config = json!({"request":{"url":"https://example.com/models?a=1","query":{"a":"2"},"auth":{"kind":"none"}},"pagination":{"kind":"none"}});
        assert!(Request::parse(&config, |_| None).is_err());
        config["request"]
            .as_object_mut()
            .ok_or_else(|| invalid("test shape"))?
            .remove("query");
        config["request"]["auth"] = json!({"kind":"bearer","env":"TEST_KEY"});
        assert_eq!(
            Request::parse(&config, |_| None).err().map(|e| e.code),
            Some("missing_secret")
        );
        config["request"]["headers"] = json!({"Authorization":"public"});
        let err = Request::parse(&config, |_| Some("secret-token".into()))
            .err()
            .ok_or_else(|| invalid("Expected collision"))?;
        assert!(!err.to_string().contains("secret-token"));
        Ok(())
    }
}
