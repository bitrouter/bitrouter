//! Explicit Agent discovery; legacy developer metadata is never a fallback.
use super::crypto;
use anyhow::{Context, Result, bail};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::Value;

pub(super) const SCOPES: &str = "agent:self:read inference:invoke offline_access";
pub(super) const DEVICE: &str = "urn:ietf:params:oauth:grant-type:device_code";
#[derive(Clone, Deserialize)]
pub(super) struct Metadata {
    pub issuer: String,
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
    pub revocation_endpoint: String,
    pub token_endpoint_auth_methods_supported: Vec<String>,
    pub dpop_signing_alg_values_supported: Vec<String>,
    pub grant_types_supported: Vec<String>,
}
#[derive(Clone, Deserialize)]
pub(super) struct Discovery {
    pub version: u8,
    pub resource: String,
    pub dpop_required: bool,
    pub nonce_required: bool,
    pub connection_client_authentication: String,
    pub registration_endpoint: String,
    pub confirmation_endpoint_template: String,
    pub disconnect_endpoint_template: String,
    pub activation_endpoint_template: String,
    pub self_endpoint: String,
    pub authorization_server_metadata: Metadata,
}
pub(super) fn origin(value: &str) -> Result<String> {
    let url = Url::parse(value).context("invalid Agent authority URL")?;
    let owned = cfg!(test)
        && std::env::var("BITROUTER_AGENT_OWNED_FIXTURE").as_deref() == Ok("owned-loopback-only")
        && url.host_str() == Some("127.0.0.1");
    if url.scheme() != "https" && !(owned && url.scheme() == "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("Agent authorities must be canonical HTTPS origins");
    }
    Ok(url.origin().ascii_serialization())
}
pub(super) fn resource(value: &str) -> Result<String> {
    let url = Url::parse(value).context("invalid Agent resource")?;
    let issuer = origin(&url.origin().ascii_serialization())?;
    endpoint(value, &issuer)?;
    Ok(url.as_str().trim_end_matches('/').into())
}
fn endpoint(value: &str, issuer: &str) -> Result<()> {
    let url = Url::parse(value).context("invalid Agent endpoint")?;
    if url.origin().ascii_serialization() != issuer
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("Agent metadata endpoint is outside the trusted issuer");
    }
    Ok(())
}
pub(super) fn client() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent(concat!("bro-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building Agent transport")
}
async fn read(mut response: reqwest::Response) -> Result<Reply> {
    let status = response.status().as_u16();
    let nonce = response
        .headers()
        .get("dpop-nonce")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 1024)
        .map(str::to_owned);
    if response.content_length().is_some_and(|size| size > 65536) {
        bail!("Agent response exceeded its bound");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("Agent response interrupted")?
    {
        if bytes.len() + chunk.len() > 65536 {
            bail!("Agent response exceeded its bound");
        }
        bytes.extend_from_slice(&chunk);
    }
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Agent endpoint returned an invalid response"))?
    };
    Ok(Reply {
        status,
        nonce,
        body,
    })
}
pub(super) async fn discover(issuer: &str, expected_resource: &str) -> Result<Discovery> {
    let issuer = origin(issuer)?;
    let resource = resource(expected_resource)?;
    let reply = read(
        client()?
            .get(format!("{issuer}/.well-known/oauth-authorization-server"))
            .send()
            .await
            .context("Agent discovery unavailable")?,
    )
    .await?;
    if reply.status != 200 {
        bail!("Agent discovery unavailable (HTTP {})", reply.status);
    }
    let value = reply
        .body
        .get("bitrouter_agent_profile")
        .cloned()
        .context("Agent profile unsupported here; developer login is a separate flow")?;
    let data: Discovery = serde_json::from_value(value).map_err(|_| {
        anyhow::anyhow!("unsupported Agent discovery format; update bro or the server")
    })?;
    let native = &data.authorization_server_metadata;
    if data.version != 1
        || data.resource != resource
        || !data.dpop_required
        || !data.nonce_required
        || data.connection_client_authentication != "private_key_jwt"
        || native.issuer != issuer
        || !native
            .token_endpoint_auth_methods_supported
            .iter()
            .any(|v| v == "private_key_jwt")
        || !native
            .dpop_signing_alg_values_supported
            .iter()
            .any(|v| v == "ES256")
        || !native.grant_types_supported.iter().any(|v| v == DEVICE)
        || !native
            .grant_types_supported
            .iter()
            .any(|v| v == "refresh_token")
    {
        bail!(
            "Agent profile or trusted resource mismatch; no developer/API-key fallback was attempted"
        );
    }
    for url in [
        &data.registration_endpoint,
        &data.self_endpoint,
        &native.device_authorization_endpoint,
        &native.token_endpoint,
        &native.revocation_endpoint,
    ] {
        endpoint(url, &issuer)?;
    }
    if data.confirmation_endpoint_template
        != format!("{issuer}/api/agent-connections/{{connection_id}}/confirm")
        || data.disconnect_endpoint_template
            != format!("{issuer}/api/agent-connections/{{client_id}}/disconnect")
        || data.activation_endpoint_template
            != format!("{issuer}/api/agent-connections/{{client_id}}/activation")
        || data.self_endpoint != format!("{issuer}/api/agent-connections/self")
    {
        bail!("unsupported Agent control endpoints");
    }
    Ok(data)
}
pub(super) struct Reply {
    pub status: u16,
    pub nonce: Option<String>,
    pub body: Value,
}
impl Reply {
    pub fn error(&self) -> &str {
        self.body
            .get("error")
            .and_then(Value::as_str)
            .filter(|value| {
                value.len() <= 80
                    && value
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
            })
            .unwrap_or("protocol_error")
    }
    pub fn require_ok(self) -> Result<Value> {
        if self.status != 200 {
            bail!(
                "Agent request failed (HTTP {}, {})",
                self.status,
                self.error()
            );
        }
        Ok(self.body)
    }
}
pub(super) struct Transport {
    client: Client,
    secret: String,
    as_nonce: Option<String>,
    rs_nonce: Option<String>,
}
enum Body<'a> {
    Json(&'a Value),
    Form(&'a [(&'a str, String)]),
}
impl Transport {
    pub fn new(secret: String) -> Result<Self> {
        Ok(Self {
            client: client()?,
            secret,
            as_nonce: None,
            rs_nonce: None,
        })
    }
    pub async fn key_action(&mut self, url: &str, body: &Value) -> Result<Reply> {
        self.send(url, "POST", Body::Json(body), None, None).await
    }
    pub async fn machine(
        &mut self,
        url: &str,
        client: &str,
        body: &[(&str, String)],
    ) -> Result<Reply> {
        self.send(url, "POST", Body::Form(body), Some(client), None)
            .await
    }
    pub async fn protected(&mut self, url: &str, method: &str, token: &str) -> Result<Reply> {
        self.send(url, method, Body::Form(&[]), None, Some(token))
            .await
    }
    async fn send(
        &mut self,
        url: &str,
        method: &str,
        body: Body<'_>,
        client: Option<&str>,
        token: Option<&str>,
    ) -> Result<Reply> {
        for attempt in 0..3 {
            let nonce = if token.is_some() {
                self.rs_nonce.as_deref()
            } else {
                self.as_nonce.as_deref()
            };
            let proof = crypto::proof(&self.secret, url, method, nonce, token)?;
            let mut request = self
                .client
                .request(method.parse()?, url)
                .header("DPoP", proof);
            if let Some(token) = token {
                request = request.header("Authorization", format!("DPoP {token}"));
            }
            match &body {
                Body::Json(value) => request = request.json(value),
                Body::Form(values) if method != "GET" => {
                    let mut values = values.to_vec();
                    if let Some(client) = client {
                        values.push(("client_id", client.into()));
                        values.push((
                            "client_assertion_type",
                            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".into(),
                        ));
                        values.push((
                            "client_assertion",
                            crypto::assertion(&self.secret, client, url)?,
                        ));
                    }
                    request = request.form(&values);
                }
                Body::Form(_) => {}
            }
            let reply = read(request.send().await.map_err(|_| {
                anyhow::anyhow!(
                    "Agent request interrupted; check the saved profile before retrying"
                )
            })?)
            .await?;
            if matches!(reply.status, 400 | 401) && reply.error() == "use_dpop_nonce" && attempt < 2
            {
                let nonce = reply
                    .nonce
                    .context("Agent nonce challenge was incomplete")?;
                if token.is_some() {
                    self.rs_nonce = Some(nonce);
                } else {
                    self.as_nonce = Some(nonce);
                }
                continue;
            }
            return Ok(reply);
        }
        bail!("Agent nonce negotiation failed")
    }
}
