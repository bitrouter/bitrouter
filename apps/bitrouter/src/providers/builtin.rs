//! The compiled-in built-in: the hosted `bitrouter` cloud gateway.
//!
//! The other known providers (openai/anthropic/google + the gateways) are NOT
//! compiled in — they come from the fetched-or-cached provider registry and are
//! configured by the registry merge ([`crate::providers::registry::apply`]). Only the
//! `bitrouter` hosted cloud *gateway* lives here: the public registry owns its
//! provider metadata, while this built-in keeps the zero-config auth/transport
//! defaults and the local cloud OAuth/API-key auth applier. [`entry_from_registry`]
//! reuses the same mapper for a fetched registry provider when a consumer (e.g.
//! `bitrouter providers login`) needs the auth/transport shape of one of those
//! providers.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::config::ProviderClass;
use bitrouter_ai::types::{ApiProtocol, ProtocolList};

use super::entry::{AuthScheme, ProtocolMapping, ProviderEntry};
use crate::gateway::catalog::types::{
    Billing, RegistryAuth, RegistryAuthKind, RegistryKind, RegistryProvider,
};

/// The hosted bitrouter cloud gateway — the sole compiled-in built-in. Public
/// provider metadata comes from the registry; this hand-authored entry carries
/// auth defaults and local OAuth/API-key integration.
const BITROUTER_TOML: &str = include_str!("../../providers/bitrouter.toml");

static REGISTRY: OnceLock<Vec<ProviderEntry>> = OnceLock::new();

/// Parse + return every compiled-in built-in entry (just the `bitrouter` cloud
/// gateway). Invalid embedded data is logged and leaves the built-in list empty;
/// the parsing regression verifies the committed data.
pub fn all() -> &'static [ProviderEntry] {
    REGISTRY
        .get_or_init(|| {
            load_builtins().unwrap_or_else(|error| {
                tracing::error!(%error, "embedded Cloud provider defaults are invalid");
                Vec::new()
            })
        })
        .as_slice()
}

/// Look up a compiled-in built-in entry by `id`. Returns `None` for unknown
/// ids — including the registry-sourced providers, which are not compiled in.
pub fn find(id: &str) -> Option<&'static ProviderEntry> {
    all().iter().find(|e| e.id == id)
}

/// Parse the compiled-in built-ins: just the `bitrouter` cloud gateway.
/// Separated from [`all`] so tests can assert on the `Result`.
pub fn load_builtins() -> Result<Vec<ProviderEntry>, LoadError> {
    let bitrouter: ProviderEntry =
        toml::from_str(BITROUTER_TOML).map_err(|source| LoadError::Parse {
            id: "bitrouter".to_string(),
            source,
        })?;
    Ok(vec![bitrouter])
}

/// Derive application defaults and login inputs from registry metadata.
/// An omitted auth block uses the provider's conventional bearer environment
/// variable. The full registry auth declaration remains with the transport.
pub fn entry_from_registry(p: &RegistryProvider) -> Result<ProviderEntry, LoadError> {
    let auth = match &p.auth {
        Some(auth) => map_auth(&p.name, auth)?,
        None => AuthScheme::Bearer {
            env: p.env_credential_var().ok_or_else(|| LoadError::Snapshot {
                message: format!("provider '{}' has no credential env var", p.name),
            })?,
        },
    };
    let api_base = p.api_base.clone().ok_or_else(|| LoadError::Snapshot {
        message: format!("provider '{}' has no fixed api_base", p.name),
    })?;
    Ok(ProviderEntry {
        id: p.name.clone(),
        display_name: p.display_name.clone().unwrap_or_else(|| p.name.clone()),
        api_base,
        api_protocol: derive_protocol_mapping(p),
        protocol_endpoints: p.protocol_endpoints.clone().unwrap_or_default(),
        auth,
        class: Some(derive_class(p)),
    })
}

/// Project credential discovery and CLI login inputs from registry auth.
/// Required header and handler declarations remain validated, while runtime
/// transports retain the complete registry declaration. Device-code login needs
/// the client id, endpoints and scope in `params`; values TOML cannot represent
/// are omitted so login can report a missing parameter.
fn map_auth(provider: &str, auth: &RegistryAuth) -> Result<AuthScheme, LoadError> {
    let missing = |field: &str| LoadError::Snapshot {
        message: format!(
            "provider '{provider}' {:?} auth missing `{field}`",
            auth.kind
        ),
    };
    match auth.kind {
        RegistryAuthKind::None => Ok(AuthScheme::None),
        RegistryAuthKind::Bearer => Ok(AuthScheme::Bearer {
            env: auth.env.clone().ok_or_else(|| missing("env"))?,
        }),
        RegistryAuthKind::Header => {
            auth.header.as_ref().ok_or_else(|| missing("header"))?;
            Ok(AuthScheme::Header {
                env: auth.env.clone().ok_or_else(|| missing("env"))?,
            })
        }
        RegistryAuthKind::Oauth => {
            auth.handler.as_ref().ok_or_else(|| missing("handler"))?;
            Ok(AuthScheme::Oauth {
                params: auth
                    .params
                    .as_ref()
                    .map(|params| {
                        params
                            .iter()
                            // serde_json::Value → toml::Value via serde; drop any
                            // value TOML can't represent (e.g. JSON null) rather
                            // than fail the whole snapshot load.
                            .filter_map(|(k, v)| {
                                toml::Value::try_from(v).ok().map(|tv| (k.clone(), tv))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        }
        RegistryAuthKind::Native => {
            auth.handler.as_ref().ok_or_else(|| missing("handler"))?;
            Ok(AuthScheme::Native)
        }
    }
}

/// Derive the wire-protocol mapping. Older dist files and explicitly
/// configured gateway providers may carry provider-level globs; complete
/// registry dist resolves protocol data onto each model, so reconstruct the
/// mapping from those entries when the globs are absent.
fn derive_protocol_mapping(p: &RegistryProvider) -> ProtocolMapping {
    let mut globs: BTreeMap<String, ProtocolList> = BTreeMap::new();
    for entry in &p.api_protocol {
        for (pattern, set) in entry {
            globs.insert(pattern.clone(), set.to_protocol_list());
        }
    }
    if !globs.is_empty() {
        return single_or_per_model(globs);
    }
    // Curated provider: rebuild from the per-model resolved protocols.
    let mut per_model: BTreeMap<String, ProtocolList> = BTreeMap::new();
    for m in &p.models {
        per_model.insert(m.id.clone(), m.api_protocol.to_protocol_list());
    }
    if per_model.is_empty() {
        return ProtocolMapping::Single(ProtocolList(vec![ApiProtocol::ChatCompletions]));
    }
    // When every model shares one protocol set, collapse to a single `*`.
    let mut values = per_model.values();
    if let Some(first) = values.next()
        && values.all(|v| v == first)
    {
        return ProtocolMapping::Single(first.clone());
    }
    ProtocolMapping::PerModel(per_model)
}

/// A lone `*` glob collapses to `Single`; anything else stays per-pattern.
fn single_or_per_model(globs: BTreeMap<String, ProtocolList>) -> ProtocolMapping {
    if globs.len() == 1
        && let Some(list) = globs.get("*")
    {
        return ProtocolMapping::Single(list.clone());
    }
    ProtocolMapping::PerModel(globs)
}

/// Derive the routing-priority class from `kind` (falling back to `community`)
/// and `billing`.
fn derive_class(p: &RegistryProvider) -> ProviderClass {
    let kind = p.kind.unwrap_or(if p.community {
        RegistryKind::ThirdParty
    } else {
        RegistryKind::FirstParty
    });
    match kind {
        RegistryKind::Cloud => ProviderClass::BitrouterCloud,
        RegistryKind::Gateway => ProviderClass::GatewaySubscription,
        RegistryKind::ThirdParty => ProviderClass::ThirdPartyApi,
        RegistryKind::FirstParty => {
            if p.billing == Billing::Subscription {
                ProviderClass::FirstPartySubscription
            } else {
                ProviderClass::FirstPartyApi
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use bitrouter_ai::types::ApiProtocol;

    #[test]
    fn only_the_cloud_gateway_is_compiled_in() -> anyhow::Result<()> {
        let entries = load_builtins().context("bitrouter.toml must parse")?;
        assert_eq!(entries.len(), 1, "only the cloud gateway is compiled in");
        assert_eq!(entries[0].id, "bitrouter");
        Ok(())
    }

    #[test]
    fn bitrouter_parses_with_bearer_env_var() -> anyhow::Result<()> {
        let entry = find("bitrouter").context("`bitrouter` must be compiled in")?;
        assert_eq!(entry.api_base, "https://api.bitrouter.ai/v1");
        assert_eq!(entry.auth.env_var(), Some("BITROUTER_API_KEY"));
        let ProtocolMapping::Single(protocols) = &entry.api_protocol else {
            anyhow::bail!("expected one protocol set");
        };
        assert_eq!(protocols.as_slice(), &[ApiProtocol::ChatCompletions]);
        assert_eq!(entry.class, Some(ProviderClass::BitrouterCloud));
        Ok(())
    }

    #[test]
    fn registry_providers_are_not_compiled_in() -> anyhow::Result<()> {
        // The known upstreams + gateways now come from the fetched-or-cached
        // registry and are configured by the merge — only the cloud gateway is
        // compiled in, so `find` does not know them.
        for id in [
            "openai",
            "anthropic",
            "google",
            "openai-codex",
            "github-copilot",
            "openrouter",
            "opencode-zen",
            "opencode-go",
            "definitely-not-a-provider",
        ] {
            assert!(find(id).is_none(), "{id} must not be compiled in");
        }
        Ok(())
    }

    fn reg(json: serde_json::Value) -> anyhow::Result<RegistryProvider> {
        Ok(serde_json::from_value(json)?)
    }

    #[test]
    fn entry_from_registry_maps_oauth_gateway() -> anyhow::Result<()> {
        // A github-copilot-shaped registry provider: provider-level protocol
        // globs + a device-code OAuth block whose `params` hold the only copy
        // of the client config. `entry_from_registry` (used by `bitrouter
        // login`) must reproduce the protocol map, the class, and — the
        // regression — carry the device-code params (PKCE providers ignore
        // them, device-code providers need them).
        let provider = reg(serde_json::json!({
            "name": "github-copilot",
            "display_name": "GitHub Copilot",
            "api_base": "https://api.githubcopilot.com",
            "kind": "gateway",
            "billing": "subscription",
            "access": "local_oauth",
            "status": "active",
            "api_protocol": [
                { "claude-*": "anthropic" },
                { "gpt-5.5-codex": "responses" },
                { "*": "openai" }
            ],
            "auth": {
                "kind": "oauth",
                "handler": "github-copilot",
                "params": {
                    "client_id": "Ov23xxx",
                    "device_authorization_endpoint": "https://github.com/login/device/code",
                    "token_endpoint": "https://github.com/login/oauth/access_token"
                }
            },
            "models": []
        }))?;
        let entry = entry_from_registry(&provider).context("maps")?;
        assert_eq!(entry.id, "github-copilot");
        assert_eq!(entry.class, Some(ProviderClass::GatewaySubscription));
        let ProtocolMapping::PerModel(protocols) = &entry.api_protocol else {
            anyhow::bail!("expected per-model protocol projection");
        };
        for (pattern, protocol) in [
            ("claude-*", ApiProtocol::Messages),
            ("gpt-5.5-codex", ApiProtocol::Responses),
            ("*", ApiProtocol::ChatCompletions),
        ] {
            assert_eq!(
                protocols
                    .get(pattern)
                    .context("protocol pattern")?
                    .as_slice(),
                &[protocol]
            );
        }
        let AuthScheme::Oauth { params, .. } = &entry.auth else {
            anyhow::bail!("github-copilot must use an OAuth scheme");
        };
        assert!(params.contains_key("client_id"));
        assert!(params.contains_key("device_authorization_endpoint"));
        assert!(params.contains_key("token_endpoint"));
        Ok(())
    }

    #[test]
    fn entry_from_registry_collapses_per_model_protocol_set() -> anyhow::Result<()> {
        // A provider with explicit model entries (no provider-level globs) whose models carry the
        // ordered [openai, responses] set: the mapping is reconstructed from
        // the models, and the bearer env var + class are derived.
        let provider = reg(serde_json::json!({
            "name": "openai",
            "api_base": "https://api.openai.com/v1",
            "kind": "first_party",
            "status": "active",
            "auth": { "kind": "bearer", "env": "OPENAI_API_KEY" },
            "models": [
                { "id": "openai/gpt-5.5", "provider_model_id": "gpt-5.5",
                  "api_protocol": ["openai", "responses"] }
            ]
        }))?;
        let entry = entry_from_registry(&provider).context("maps")?;
        assert_eq!(entry.auth.env_var(), Some("OPENAI_API_KEY"));
        assert_eq!(entry.class, Some(ProviderClass::FirstPartyApi));
        let ProtocolMapping::Single(protocols) = &entry.api_protocol else {
            anyhow::bail!("expected collapsed protocol set");
        };
        assert_eq!(
            protocols.as_slice(),
            &[ApiProtocol::ChatCompletions, ApiProtocol::Responses]
        );
        Ok(())
    }

    #[test]
    fn entry_from_registry_uses_implicit_bearer_auth() -> Result<(), Box<dyn std::error::Error>> {
        let provider: RegistryProvider = serde_json::from_value(serde_json::json!({
            "name": "example-provider",
            "api_base": "https://example.test/v1",
            "status": "active",
            "models": []
        }))?;
        let entry = entry_from_registry(&provider)?;
        assert!(
            matches!(entry.auth, AuthScheme::Bearer { env } if env == "EXAMPLE_PROVIDER_API_KEY")
        );
        Ok(())
    }
}

/// Errors raised while loading the compile-time registry.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The compiled-in `bitrouter.toml` failed to parse. The string carries
    /// the provider id + the underlying `toml` error.
    #[error("failed to parse provider entry '{id}': {source}")]
    Parse {
        /// The id (filename stem) of the entry that failed.
        id: String,
        /// The underlying TOML parse error.
        #[source]
        source: toml::de::Error,
    },
    /// A registry provider could not be mapped to a [`ProviderEntry`] — e.g. it
    /// declared no `auth` block, or an auth scheme missing a required field (a
    /// `bearer` scheme with no `env`). Surfaced by [`entry_from_registry`].
    #[error("invalid registry provider: {message}")]
    Snapshot {
        /// What was wrong with the provider.
        message: String,
    },
}
