//! Remote request-checker declarations.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{BitrouterError, Result};

/// The only request-checker wire contract supported by this release.
pub const CONTRACT_VERSION: u16 = bitrouter_checker_protocol::v1::CONTRACT_VERSION;

/// One remotely hosted request checker.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckerConfig {
    /// Exact HTTP endpoint receiving versioned checker invocations.
    pub endpoint: String,
    /// Dedicated bearer credential environment variable, if authentication is required.
    pub credential_env: Option<String>,
    /// Version of the checker request and response contract.
    pub contract_version: u16,
}

impl std::fmt::Debug for CheckerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CheckerConfig")
            .field("endpoint", &"<redacted>")
            .field("credential_env", &self.credential_env)
            .field("contract_version", &self.contract_version)
            .finish()
    }
}

impl CheckerConfig {
    pub(super) fn validate(&self, checker_id: &str) -> Result<()> {
        let endpoint = Url::parse(&self.endpoint).map_err(|_| {
            BitrouterError::bad_request(format!(
                "checker '{checker_id}' endpoint must be a valid absolute HTTP URL"
            ))
        })?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err(BitrouterError::bad_request(format!(
                "checker '{checker_id}' endpoint must be an absolute HTTP or HTTPS URL"
            )));
        }
        if !endpoint.username().is_empty() || endpoint.password().is_some() {
            return Err(BitrouterError::bad_request(format!(
                "checker '{checker_id}' endpoint must not contain user information"
            )));
        }
        if endpoint.fragment().is_some() {
            return Err(BitrouterError::bad_request(format!(
                "checker '{checker_id}' endpoint must not contain a fragment"
            )));
        }
        if self.contract_version != CONTRACT_VERSION {
            return Err(BitrouterError::bad_request(format!(
                "checker '{checker_id}' contract_version must be {CONTRACT_VERSION}"
            )));
        }
        if self
            .credential_env
            .as_deref()
            .is_some_and(|name| !valid_env_name(name))
        {
            return Err(BitrouterError::bad_request(format!(
                "checker '{checker_id}' credential_env must name an environment variable"
            )));
        }
        Ok(())
    }
}

fn valid_env_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes
        .first()
        .is_some_and(|first| first.is_ascii_alphabetic() || *first == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}
