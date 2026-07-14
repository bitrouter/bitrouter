//! Raw BitRouter Cloud HTTP API client.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode, Version};
use tokio::sync::Mutex;
use url::Url;

use crate::auth::credentials::{CredentialsStore, REFRESH_WINDOW, default_credentials_path};
use crate::auth::metadata::{self, AsMetadata};

/// A raw HTTP request to a relative BitRouter Cloud endpoint.
pub struct ApiRequest {
    method: Method,
    endpoint: String,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
}

impl std::fmt::Debug for ApiRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let header_names = self.headers.keys().collect::<Vec<_>>();
        let body = self
            .body
            .as_ref()
            .map(|bytes| format!("<redacted> ({} bytes)", bytes.len()));
        f.debug_struct("ApiRequest")
            .field("method", &self.method)
            .field("endpoint", &self.endpoint)
            .field("header_names", &header_names)
            .field("body", &body)
            .finish()
    }
}

impl ApiRequest {
    /// Create a request with no custom headers or body.
    pub fn new(method: Method, endpoint: impl Into<String>) -> Self {
        Self {
            method,
            endpoint: endpoint.into(),
            headers: HeaderMap::new(),
            body: None,
        }
    }

    /// Replace the request's custom headers.
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    /// Attach an exact byte body to the request.
    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// Return the configured HTTP method.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Return the relative endpoint supplied by the caller.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Return the custom request headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }
}

/// A raw HTTP response whose body remains available as a reqwest stream.
#[derive(Debug)]
pub struct ApiResponse {
    response: reqwest::Response,
}

impl ApiResponse {
    /// Return the response status without consuming the body.
    pub fn status(&self) -> StatusCode {
        self.response.status()
    }

    /// Return the HTTP protocol version.
    pub fn version(&self) -> Version {
        self.response.version()
    }

    /// Return the response headers.
    pub fn headers(&self) -> &HeaderMap {
        self.response.headers()
    }

    /// Return the final response URL. Redirect following is disabled, so this
    /// is always the confined request URL.
    pub fn url(&self) -> &Url {
        self.response.url()
    }

    /// Consume the wrapper and return the streaming reqwest response.
    pub fn into_response(self) -> reqwest::Response {
        self.response
    }
}

/// Authenticated client for arbitrary relative BitRouter Cloud endpoints.
pub struct CloudApiClient {
    base_url: Url,
    http: reqwest::Client,
    store: Arc<Mutex<CredentialsStore>>,
    metadata: Arc<Mutex<Option<AsMetadata>>>,
}

impl std::fmt::Debug for CloudApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudApiClient")
            .field("base_url", &self.base_url)
            .field("credentials", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl CloudApiClient {
    /// Build a client from the default `bitrouter cloud login` credential.
    pub fn from_default_credentials() -> Result<Self> {
        Self::from_credentials_path(default_credentials_path()?)
    }

    /// Build a client from an explicit credentials file.
    pub fn from_credentials_path(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let store = CredentialsStore::load(&path)
            .with_context(|| format!("reading credentials at {}", path.display()))?;
        let credential = store
            .current()
            .context("no stored credentials — run `bitrouter cloud login` first")?;
        let base_url = Url::parse(credential.base_url()).with_context(|| {
            format!(
                "parsing the BitRouter Cloud URL stored in {}",
                path.display()
            )
        })?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("bitrouter-cloud-sdk/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building BitRouter Cloud API client")?;
        Ok(Self {
            base_url,
            http,
            store: Arc::new(Mutex::new(store)),
            metadata: Arc::new(Mutex::new(None)),
        })
    }

    /// Return the stored login URL used as the request origin.
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Resolve a relative endpoint against the stored login origin.
    pub fn endpoint_url(&self, endpoint: &str) -> Result<Url> {
        resolve_endpoint(&self.base_url, endpoint)
    }

    /// Send one authenticated request without interpreting its status or
    /// buffering its response body.
    pub async fn execute(&self, request: ApiRequest) -> Result<ApiResponse> {
        let url = self.endpoint_url(&request.endpoint)?;
        let mut headers = request.headers;
        if !headers.contains_key(AUTHORIZATION) {
            let bearer = self.current_bearer().await?;
            let value = HeaderValue::from_str(&format!("Bearer {bearer}"))
                .context("stored credential cannot be represented as an HTTP header")?;
            headers.insert(AUTHORIZATION, value);
        }
        if !headers.contains_key(ACCEPT) {
            headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        }
        let mut builder = self.http.request(request.method, url).headers(headers);
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        let response = builder
            .send()
            .await
            .context("sending BitRouter Cloud request")?;
        Ok(ApiResponse { response })
    }

    async fn current_bearer(&self) -> Result<String> {
        let mut store = self.store.lock().await;
        let refresh_url = store
            .current()
            .and_then(|credential| credential.oauth())
            .filter(|credentials| credentials.access_token_near_expiry(REFRESH_WINDOW))
            .map(|credentials| credentials.authorization_server.clone());
        let metadata = match refresh_url {
            Some(url) => Some(self.resolve_metadata(&url).await?),
            None => None,
        };
        store.current_token(&self.http, metadata.as_ref()).await
    }

    async fn resolve_metadata(&self, authorization_server: &str) -> Result<AsMetadata> {
        let mut cached = self.metadata.lock().await;
        if let Some(metadata) = cached.as_ref() {
            return Ok(metadata.clone());
        }
        let fetched = metadata::fetch(&self.http, authorization_server)
            .await
            .with_context(|| format!("fetching AS metadata at {authorization_server}"))?;
        *cached = Some(fetched.clone());
        Ok(fetched)
    }
}

fn resolve_endpoint(base_url: &Url, endpoint: &str) -> Result<Url> {
    if endpoint.is_empty() {
        anyhow::bail!("API endpoint cannot be empty");
    }
    if endpoint.starts_with("//") || endpoint.starts_with("\\\\") || Url::parse(endpoint).is_ok() {
        anyhow::bail!("API endpoint must be a relative path on the logged-in origin");
    }
    let mut origin = base_url.clone();
    origin.set_path("/");
    origin.set_query(None);
    origin.set_fragment(None);
    let resolved = origin
        .join(endpoint)
        .with_context(|| format!("resolving API endpoint '{endpoint}'"))?;
    if resolved.origin() != origin.origin() {
        anyhow::bail!("API endpoint resolves outside the logged-in origin");
    }
    if resolved.fragment().is_some() {
        anyhow::bail!("API endpoint fragments are not supported");
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::credentials::{CredentialsStore, StoredCredential};

    fn tmp_credentials_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-cloud-api-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("account-credentials.json")
    }

    fn save_api_key(path: &std::path::Path, base_url: &str) {
        let mut store = CredentialsStore::load(path).unwrap();
        store
            .save(StoredCredential::api_key(
                "brk_AAAAAAAAAAAAAAAA.secret".to_owned(),
                base_url.to_owned(),
            ))
            .unwrap();
    }

    #[test]
    fn resolves_only_relative_paths_on_the_login_origin() {
        let base = url::Url::parse("https://api.bitrouter.ai/oauth").unwrap();
        assert_eq!(
            resolve_endpoint(&base, "/v1/models?owned=true")
                .unwrap()
                .as_str(),
            "https://api.bitrouter.ai/v1/models?owned=true"
        );
        assert_eq!(
            resolve_endpoint(&base, "v1/models").unwrap().as_str(),
            "https://api.bitrouter.ai/v1/models"
        );
        for endpoint in [
            "https://evil.example/v1/models",
            "//evil.example/v1/models",
            "\\\\evil.example/v1/models",
            "/v1/models#fragment",
            "mailto:security@example.com",
        ] {
            assert!(
                resolve_endpoint(&base, endpoint).is_err(),
                "{endpoint} must be rejected"
            );
        }
    }

    #[test]
    fn api_request_debug_redacts_headers_and_body() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer request-secret"),
        );
        let request = ApiRequest::new(reqwest::Method::POST, "/v1/responses")
            .with_headers(headers)
            .with_body(br#"{"input":"private prompt"}"#.to_vec());

        let rendered = format!("{request:?}");

        assert!(!rendered.contains("request-secret"));
        assert!(!rendered.contains("private prompt"));
        assert!(rendered.contains("<redacted>"));
    }

    #[tokio::test]
    async fn sends_stored_api_key_and_preserves_response_stream() {
        let server = MockServer::start().await;
        let credentials_path = tmp_credentials_path("request");
        save_api_key(&credentials_path, &server.uri());
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header(
                "authorization",
                "Bearer brk_AAAAAAAAAAAAAAAA.secret",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("{\"data\":[]}", "application/json"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = CloudApiClient::from_credentials_path(credentials_path).unwrap();
        let response = client
            .execute(ApiRequest::new(reqwest::Method::GET, "/v1/models"))
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.into_response().text().await.unwrap(),
            "{\"data\":[]}"
        );
    }

    #[tokio::test]
    async fn does_not_follow_cross_origin_redirects() {
        let destination = MockServer::start().await;
        let origin = MockServer::start().await;
        let credentials_path = tmp_credentials_path("redirect");
        save_api_key(&credentials_path, &origin.uri());
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/stolen", destination.uri())),
            )
            .expect(1)
            .mount(&origin)
            .await;

        let client = CloudApiClient::from_credentials_path(credentials_path).unwrap();
        let response = client
            .execute(ApiRequest::new(reqwest::Method::GET, "/v1/models"))
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        assert!(
            destination
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn user_authorization_overrides_default_and_repeated_headers_survive() {
        let server = MockServer::start().await;
        let credentials_path = tmp_credentials_path("headers");
        save_api_key(&credentials_path, &server.uri());
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer user-override"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer user-override"),
        );
        headers.append("x-test", HeaderValue::from_static("one"));
        headers.append("x-test", HeaderValue::from_static("two"));

        let client = CloudApiClient::from_credentials_path(credentials_path).unwrap();
        client
            .execute(ApiRequest::new(reqwest::Method::GET, "/v1/models").with_headers(headers))
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap_or_default();
        let values = requests[0]
            .headers
            .get_all("x-test")
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values, ["one", "two"]);
    }
}
