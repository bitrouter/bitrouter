//! HTTP and explicitly compiled-in request-checker declarations.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{BitrouterError, Result};

/// The only request-checker wire contract supported by this release.
pub const CONTRACT_VERSION: u16 = bitrouter_checker_protocol::v1::CONTRACT_VERSION;

/// A request-check instance, supplied over HTTP or explicitly linked by a custom host.
/// Existing HTTP configuration keeps its untagged wire shape.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum CheckerConfig {
    /// An external service implementing request-check v1.
    Http {
        /// Exact HTTP endpoint receiving versioned checker invocations.
        endpoint: String,
        /// Dedicated bearer credential environment variable.
        credential_env: Option<String>,
        /// Version of the request and response contract.
        contract_version: u16,
    },
    /// A callback explicitly registered under this checker id by a custom host.
    Native {
        /// Expected code/rules revision; registration must match exactly.
        native: NativeCheckerConfig,
    },
}

/// Startup identity for trusted, compiled-in request-check code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NativeCheckerConfig {
    /// Operator-chosen code/rules revision. Change it when behavior changes.
    pub revision: String,
}

impl std::fmt::Debug for CheckerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http {
                credential_env,
                contract_version,
                ..
            } => formatter
                .debug_struct("HttpChecker")
                .field("endpoint", &"<redacted>")
                .field("credential_env", credential_env)
                .field("contract_version", contract_version)
                .finish(),
            Self::Native { native } => formatter
                .debug_tuple("NativeChecker")
                .field(native)
                .finish(),
        }
    }
}

impl CheckerConfig {
    pub(super) fn validate(&self, checker_id: &str) -> Result<()> {
        let (endpoint, credential_env, contract_version) = match self {
            Self::Native { native } => {
                if native.revision.is_empty()
                    || native.revision.len() > 128
                    || !native.revision.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':' | b'/')
                    })
                {
                    return Err(BitrouterError::bad_request(format!(
                        "checker '{checker_id}' native revision must be a bounded machine-readable identifier"
                    )));
                }
                return Ok(());
            }
            Self::Http {
                endpoint,
                credential_env,
                contract_version,
            } => (endpoint, credential_env, contract_version),
        };
        let endpoint = Url::parse(endpoint).map_err(|_| {
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
        if *contract_version != CONTRACT_VERSION {
            return Err(BitrouterError::bad_request(format!(
                "checker '{checker_id}' contract_version must be {CONTRACT_VERSION}"
            )));
        }
        if credential_env
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
