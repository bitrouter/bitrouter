//! Explicit privileged OAuth lineage, not a tenant wire authority.
use crate::gateway::providers::{auth::credentials::OAuthToken, login};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub(in crate::tenancy) const TOKEN_BYTES: usize = 64 * 1024;
pub(in crate::tenancy) const TOKEN_JSON_BYTES: usize = 256 * 1024;
pub(in crate::tenancy) const PAYLOAD_BYTES: usize = 512 * 1024;
pub(in crate::tenancy) const MAX_SLOTS: usize = 256;
pub(in crate::tenancy) const METADATA_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Profile {
    pub format: u32,
    pub mechanism: String,
    pub provider: String,
    pub account: String,
    pub registration: String,
    /// The import pins exact known registration values, never a discovery URL.
    pub token_endpoint: String,
    pub client_id: String,
}
impl Profile {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format == 1
                && self.mechanism == "claude_code_refresh_v1"
                && self.provider == "claude-code"
                && self.registration == "anthropic"
                && !self.account.is_empty()
                && self.account.len() <= 128,
            "unsupported owned OAuth profile"
        );
        let registration = login::find(&self.registration)
            .ok_or_else(|| anyhow::anyhow!("owned OAuth registration unavailable"))?;
        let exact = self.token_endpoint == registration.auth.token_endpoint
            && self.client_id == registration.auth.client_id;
        #[cfg(test)]
        let exact = exact
            || (self.client_id == registration.auth.client_id
                && reqwest::Url::parse(&self.token_endpoint).is_ok_and(|url| {
                    url.scheme() == "http"
                        && url.host_str().is_some_and(|host| {
                            host.parse::<std::net::IpAddr>()
                                .is_ok_and(|ip| ip.is_loopback())
                        })
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.query().is_none()
                        && url.fragment().is_none()
                }));
        ensure!(exact, "owned OAuth registration mismatch");
        ensure!(
            serde_json::to_vec(self)?.len() <= 2048,
            "OAuth profile byte bound"
        );
        Ok(())
    }
}
pub(in crate::tenancy) fn validate_token(token: &OAuthToken) -> Result<()> {
    ensure!(
        !token.access_token.is_empty()
            && token.access_token.len() <= TOKEN_BYTES
            && token
                .refresh_token
                .as_ref()
                .is_some_and(|value| !value.is_empty() && value.len() <= TOKEN_BYTES)
            && token.expires_at > 0
            && token.expires_at <= i64::MAX as u64,
        "invalid owned OAuth token"
    );
    let mut header =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.access_token))?;
    header.set_sensitive(true);
    ensure!(
        serde_json::to_vec(token)?.len() <= TOKEN_JSON_BYTES,
        "OAuth token byte bound"
    );
    Ok(())
}
