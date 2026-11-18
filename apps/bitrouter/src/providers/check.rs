//! Read-only model-list check for one configured provider.
//!
//! The check resolves the provider's endpoint and credential, then GETs the
//! protocol's model catalog with the gateway HTTP client, timeouts, and
//! authentication. It does not submit a generation, take scheduling capacity,
//! change `active`, or write configuration. Models it sees stay in the result.

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, ProviderConfig};
use crate::gateway::error::{BitrouterError, Result};
use crate::gateway::model::client::{HttpTimeouts, ModelClient};
use crate::gateway::model::error::ModelError;
use crate::gateway::model::target::{AuthScheme, CredentialPriority, ModelTarget};
use crate::output::CliReport;
use crate::output::human::{Human, Table};
use bitrouter_ai::types::{ApiProtocol, ModelCompatibility};

const UNRESOLVED_MARKER: &str = "http://127.0.0.1/bitrouter-unresolved-ref/";

/// One configured connection inside a provider check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderConnectionCheck {
    /// Provider id, or `provider/account` when the provider has accounts.
    pub id: String,
    /// Resolved endpoint. Empty when that reference did not resolve.
    pub endpoint: String,
    /// Protocol used for the list request. Empty when the endpoint did not resolve.
    pub protocol: String,
    /// Whether the endpoint returned this protocol's model list.
    pub answered: bool,
    /// Model ids in the list. Empty when the endpoint did not answer.
    pub models: Vec<String>,
    /// Why the list was not accepted. Absent when `answered` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of checking one configured provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderCheckReport {
    /// Provider id the operator named.
    pub provider: String,
    /// One row per connection. A provider without accounts has one row.
    pub connections: Vec<ProviderConnectionCheck>,
}

impl CliReport for ProviderCheckReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        let mut table = Table::new(["ID", "PROTOCOL", "ANSWERED", "MODELS"]);
        for connection in &self.connections {
            table.push([
                connection.id.clone(),
                if connection.protocol.is_empty() {
                    "—".into()
                } else {
                    connection.protocol.clone()
                },
                if connection.answered {
                    "yes".into()
                } else {
                    "no".into()
                },
                if connection.answered {
                    connection.models.join(", ")
                } else {
                    connection.error.clone().unwrap_or_default()
                },
            ]);
        }
        h.table(&table)
    }

    fn exit_code(&self) -> i32 {
        if self
            .connections
            .iter()
            .all(|connection| connection.answered)
        {
            0
        } else {
            1
        }
    }
}

struct Slot {
    id: String,
    provider_name: String,
    api_base: String,
    api_key: String,
    account_label: Option<String>,
    protocol: ApiProtocol,
}

/// Check a YAML document without writing it back.
///
/// `lookup` resolves `${VAR}` the same way config load does. A name it does
/// not resolve fails only the connection that uses it, and that connection
/// is not dialed.
pub async fn check_document<F>(
    yaml: &str,
    provider_id: &str,
    account: Option<&str>,
    lookup: F,
) -> Result<ProviderCheckReport>
where
    F: Fn(&str) -> Option<String>,
{
    let document =
        crate::config::substitute_with_missing(yaml, lookup, |name| Ok(unresolved_marker(name)))?;
    let config = crate::config::parse_resolved(&document)?;
    check_loaded(&config, provider_id, account).await
}

/// Check an already loaded configuration. The input is not modified.
pub async fn check_loaded(
    config: &Config,
    provider_id: &str,
    account: Option<&str>,
) -> Result<ProviderCheckReport> {
    if let Some(message) = crate::gateway::providers::retired::provider_message(provider_id) {
        return Ok(ProviderCheckReport {
            provider: provider_id.to_string(),
            connections: vec![failed_row(provider_id, "", "", message)],
        });
    }
    let mut resolved = config.clone();
    crate::providers::apply::apply_builtin_defaults(&mut resolved);
    let Some(provider) = resolved.providers.get(provider_id) else {
        return Err(BitrouterError::bad_request(format!(
            "no provider configured for '{provider_id}'"
        )));
    };
    let slots = connections(provider_id, provider, account)?;
    let headers = static_headers(provider)?;
    let timeouts = provider
        .timeouts
        .apply_to(resolved.upstream.timeouts.apply_to(HttpTimeouts::default()));
    let mut rows = Vec::with_capacity(slots.len());
    for slot in &slots {
        rows.push(probe(slot, &headers, timeouts.clone()).await);
    }
    Ok(ProviderCheckReport {
        provider: provider_id.to_string(),
        connections: rows,
    })
}

fn unresolved_marker(name: &str) -> String {
    format!("{UNRESOLVED_MARKER}{name}")
}

fn unresolved_reference(value: &str) -> Option<&str> {
    let rest = value.split(UNRESOLVED_MARKER).nth(1)?;
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    let name = &rest[..end];
    if name.is_empty() { None } else { Some(name) }
}

fn connections(
    provider_id: &str,
    provider: &ProviderConfig,
    account: Option<&str>,
) -> Result<Vec<Slot>> {
    let protocol = provider.protocol_for("");
    if provider.accounts.is_empty() {
        if let Some(label) = account {
            return Err(BitrouterError::bad_request(format!(
                "provider '{provider_id}' has no account '{label}'"
            )));
        }
        return Ok(vec![Slot {
            id: provider_id.to_string(),
            provider_name: provider_id.to_string(),
            api_base: provider_endpoint(provider, &protocol),
            api_key: provider.api_key.clone(),
            account_label: None,
            protocol,
        }]);
    }
    let mut slots = Vec::new();
    for (index, configured) in provider.accounts.iter().enumerate() {
        let label = if configured.label.is_empty() {
            format!("account-{}", index + 1)
        } else {
            configured.label.clone()
        };
        if account.is_some_and(|wanted| wanted != label) {
            continue;
        }
        let api_base = if configured.api_base.is_empty() {
            provider_endpoint(provider, &protocol)
        } else {
            configured.api_base.clone()
        };
        slots.push(Slot {
            id: format!("{provider_id}/{label}"),
            provider_name: provider_id.to_string(),
            api_base,
            api_key: configured.api_key.clone(),
            account_label: Some(label),
            protocol: protocol.clone(),
        });
    }
    if !slots.is_empty() {
        return Ok(slots);
    }
    let Some(label) = account else {
        return Err(BitrouterError::bad_request(format!(
            "provider '{provider_id}' has no accounts"
        )));
    };
    Err(BitrouterError::bad_request(format!(
        "provider '{provider_id}' has no account '{label}'"
    )))
}

fn provider_endpoint(provider: &ProviderConfig, protocol: &ApiProtocol) -> String {
    provider
        .endpoint_for(protocol)
        .unwrap_or(provider.api_base.as_str())
        .to_string()
}

fn static_headers(provider: &ProviderConfig) -> Result<http::HeaderMap> {
    let mut headers = http::HeaderMap::new();
    for rule in provider.outbound_headers()? {
        if let Some(value) = rule.default() {
            headers.insert(rule.name().clone(), value.clone());
        }
    }
    Ok(headers)
}

async fn probe(
    slot: &Slot,
    headers: &http::HeaderMap,
    timeouts: HttpTimeouts,
) -> ProviderConnectionCheck {
    if let Some(name) = unresolved_reference(&slot.api_base) {
        return failed_row(
            &slot.id,
            "",
            "",
            &format!("endpoint reference '${{{name}}}' could not be resolved"),
        );
    }
    if slot.api_base.is_empty() {
        return failed_row(&slot.id, "", "", "endpoint is not configured");
    }
    let protocol = slot.protocol.as_str();
    if !probe_endpoint_allowed(&slot.api_base) {
        return failed_row(&slot.id, "", protocol, "endpoint is not allowed");
    }
    if let Some(name) = unresolved_reference(&slot.api_key) {
        return failed_row(
            &slot.id,
            &slot.api_base,
            protocol,
            &format!("credential reference '${{{name}}}' could not be resolved"),
        );
    }
    let target = ModelTarget {
        provider_name: slot.provider_name.clone(),
        service_id: slot.id.clone(),
        api_protocol: slot.protocol.clone(),
        api_base: slot.api_base.clone(),
        api_key: slot.api_key.clone(),
        credential_priority: CredentialPriority::Fallback,
        account_label: slot.account_label.clone(),
        auth_scheme: AuthScheme::default(),
        compatibility: ModelCompatibility::default(),
    };
    let client = match ModelClient::for_model_list_probe(timeouts) {
        Ok(client) => client,
        Err(error) => {
            return failed_row(
                &slot.id,
                &slot.api_base,
                protocol,
                &public_failure(&error, &slot.api_key),
            );
        }
    };
    let cancellation = CancellationToken::new();
    match client.list_models(&target, headers, &cancellation).await {
        Ok(models) => ProviderConnectionCheck {
            id: slot.id.clone(),
            endpoint: slot.api_base.clone(),
            protocol: protocol.to_string(),
            answered: true,
            models,
            error: None,
        },
        Err(error) => failed_row(
            &slot.id,
            &slot.api_base,
            protocol,
            &public_failure(&error, &slot.api_key),
        ),
    }
}

fn probe_endpoint_allowed(endpoint: &str) -> bool {
    crate::config::url_validator::validate_upstream_url(endpoint).is_ok()
        && url::Url::parse(endpoint).is_ok_and(|url| {
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
}

fn failed_row(id: &str, endpoint: &str, protocol: &str, error: &str) -> ProviderConnectionCheck {
    ProviderConnectionCheck {
        id: id.to_string(),
        endpoint: endpoint.to_string(),
        protocol: protocol.to_string(),
        answered: false,
        models: Vec::new(),
        error: Some(error.to_string()),
    }
}

fn public_failure(error: &ModelError, api_key: &str) -> String {
    let text = match error {
        ModelError::HttpResponse {
            status: status @ (401 | 403),
            ..
        } if api_key.is_empty() => format!(
            "endpoint returned HTTP {status}. Use a configured API key or static authentication header for this check."
        ),
        ModelError::HttpResponse { status, .. } => format!("endpoint returned HTTP {status}"),
        ModelError::Decode { message } | ModelError::Configuration { message } => message.clone(),
        ModelError::Timeout => "timed out before the endpoint answered".into(),
        ModelError::Transport { message } => format!("could not reach the endpoint: {message}"),
        other => other.to_string(),
    };
    if api_key.is_empty() {
        text
    } else {
        text.replace(api_key, "[redacted]")
    }
}
