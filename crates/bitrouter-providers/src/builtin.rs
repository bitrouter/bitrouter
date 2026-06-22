//! The compiled-in built-in: the hosted `bitrouter` cloud gateway.
//!
//! The other known providers (openai/anthropic/google + the gateways) are NOT
//! compiled in — they come from the fetched-or-cached provider registry and are
//! configured by the registry merge ([`crate::registry::apply`]). Only the
//! `bitrouter` hosted cloud *gateway* lives here: its id shadows the registry's
//! pool entry and it is the cloud-applier / serves-all-canonical mechanism, so
//! it cannot be a public-registry entry. [`entry_from_registry`] reuses the same
//! mapper for a fetched registry provider when a consumer (e.g. `bitrouter
//! login`) needs the auth/transport shape of one of those providers.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use bitrouter_sdk::config::ProviderClass;
use bitrouter_sdk::language_model::types::{ApiProtocol, ProtocolList};

use crate::LoadError;
use crate::entry::{AuthScheme, ProtocolMapping, ProviderEntry};
use crate::registry::types::{
    Billing, RegistryAuth, RegistryAuthKind, RegistryKind, RegistryProvider,
};

/// The hosted bitrouter cloud gateway — the sole compiled-in built-in. Its id
/// shadows the registry's pool entry, so it is hand-authored here rather than
/// taken from the registry.
const BITROUTER_TOML: &str = include_str!("../providers/bitrouter.toml");

static REGISTRY: OnceLock<Vec<ProviderEntry>> = OnceLock::new();

/// Parse + return every compiled-in built-in entry (just the `bitrouter` cloud
/// gateway). Panics only if the compiled-in TOML is malformed — a build-time
/// invariant caught by `cargo test`, never a user error.
pub fn all() -> &'static [ProviderEntry] {
    REGISTRY
        .get_or_init(|| load_builtins().expect("built-in provider registry must parse"))
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

/// Derive a [`ProviderEntry`] (auth + transport shape) from a registry provider
/// — the same mapping the built-ins once used, now applied to a fetched-or-
/// cached registry entry. Used where a consumer needs the auth shape of a
/// registry-sourced provider without it being compiled in (e.g. `bitrouter
/// login <provider>` resolving an OAuth handler + its public params). Errors if
/// the provider declares no `auth` block.
pub fn entry_from_registry(p: &RegistryProvider) -> Result<ProviderEntry, LoadError> {
    let auth = p.auth.as_ref().ok_or_else(|| LoadError::Snapshot {
        message: format!("provider '{}' has no auth", p.name),
    })?;
    Ok(ProviderEntry {
        id: p.name.clone(),
        display_name: p.display_name.clone().unwrap_or_else(|| p.name.clone()),
        api_base: p.api_base.clone(),
        api_protocol: derive_protocol_mapping(p),
        protocol_endpoints: p.protocol_endpoints.clone().unwrap_or_default(),
        auth: map_auth(&p.name, auth)?,
        doc_url: p.doc_url.clone().unwrap_or_default(),
        class: Some(derive_class(p)),
    })
}

/// Map the registry's structured auth declaration onto the OSS [`AuthScheme`].
/// Only public config travels (names/handlers); OAuth/native handler *impls*
/// stay in the OSS, keyed by the handler name. OAuth `params` ARE carried onto
/// the entry: PKCE providers (anthropic, openai-codex) ignore them (the OSS
/// `oauth::registry` holds their client config), but device-code providers
/// (github-copilot) keep their `client_id` / `device_authorization_endpoint` /
/// `token_endpoint` / `scope` ONLY here, so dropping them breaks
/// `bitrouter login github-copilot`. JSON values that TOML cannot represent
/// (e.g. `null`) are skipped — login then surfaces a clear "missing param".
fn map_auth(provider: &str, auth: &RegistryAuth) -> Result<AuthScheme, LoadError> {
    let missing = |field: &str| LoadError::Snapshot {
        message: format!(
            "provider '{provider}' {:?} auth missing `{field}`",
            auth.kind
        ),
    };
    match auth.kind {
        RegistryAuthKind::Bearer => Ok(AuthScheme::Bearer {
            env: auth.env.clone().ok_or_else(|| missing("env"))?,
        }),
        RegistryAuthKind::Header => Ok(AuthScheme::Header {
            header: auth.header.clone().ok_or_else(|| missing("header"))?,
            env: auth.env.clone().ok_or_else(|| missing("env"))?,
            extra_headers: auth.extra_headers.clone().unwrap_or_default(),
        }),
        RegistryAuthKind::Oauth => Ok(AuthScheme::Oauth {
            handler: auth.handler.clone().ok_or_else(|| missing("handler"))?,
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
        }),
        RegistryAuthKind::Native => Ok(AuthScheme::Native {
            handler: auth.handler.clone().ok_or_else(|| missing("handler"))?,
        }),
    }
}

/// Derive the wire-protocol mapping. A runtime-discovered provider carries
/// provider-level globs (kept in the dist); a curated provider's protocol was
/// resolved onto its models, so reconstruct the mapping from them.
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
    use bitrouter_sdk::language_model::types::ApiProtocol;

    #[test]
    fn only_the_cloud_gateway_is_compiled_in() {
        let entries = load_builtins().expect("bitrouter.toml must parse");
        assert_eq!(entries.len(), 1, "only the cloud gateway is compiled in");
        assert_eq!(entries[0].id, "bitrouter");
    }

    #[test]
    fn bitrouter_parses_with_bearer_env_var() {
        let entry = find("bitrouter").expect("`bitrouter` must be compiled in");
        assert_eq!(entry.api_base, "https://api.bitrouter.ai/v1");
        assert_eq!(entry.auth.env_var(), Some("BITROUTER_API_KEY"));
        assert_eq!(
            entry.api_protocol.resolve("gpt-4o"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
        assert_eq!(entry.class, Some(ProviderClass::BitrouterCloud));
    }

    #[test]
    fn registry_providers_are_not_compiled_in() {
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
    }

    fn reg(json: serde_json::Value) -> RegistryProvider {
        serde_json::from_value(json).expect("valid RegistryProvider fixture")
    }

    #[test]
    fn entry_from_registry_maps_oauth_gateway() {
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
        }));
        let entry = entry_from_registry(&provider).expect("maps");
        assert_eq!(entry.id, "github-copilot");
        assert_eq!(entry.class, Some(ProviderClass::GatewaySubscription));
        // Provider-level globs resolve per model.
        assert_eq!(
            entry.api_protocol.resolve("claude-sonnet-4.6"),
            Some(vec![ApiProtocol::Messages])
        );
        assert_eq!(
            entry.api_protocol.resolve("gpt-5.5-codex"),
            Some(vec![ApiProtocol::Responses])
        );
        assert_eq!(
            entry.api_protocol.resolve("gpt-4o"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
        let AuthScheme::Oauth { params, .. } = &entry.auth else {
            panic!("github-copilot must use an OAuth scheme");
        };
        assert!(params.contains_key("client_id"));
        assert!(params.contains_key("device_authorization_endpoint"));
        assert!(params.contains_key("token_endpoint"));
    }

    #[test]
    fn entry_from_registry_collapses_per_model_protocol_set() {
        // A curated provider (no provider-level globs) whose models carry the
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
        }));
        let entry = entry_from_registry(&provider).expect("maps");
        assert_eq!(entry.auth.env_var(), Some("OPENAI_API_KEY"));
        assert_eq!(entry.class, Some(ProviderClass::FirstPartyApi));
        assert_eq!(
            entry.api_protocol.resolve("gpt-5.5"),
            Some(vec![ApiProtocol::ChatCompletions, ApiProtocol::Responses])
        );
    }

    #[test]
    fn entry_from_registry_rejects_provider_without_auth() {
        let provider = reg(serde_json::json!({
            "name": "noauth",
            "api_base": "https://noauth.test/v1",
            "status": "active",
            "models": []
        }));
        assert!(entry_from_registry(&provider).is_err());
    }
}
