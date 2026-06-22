//! Compile-time registry of built-in provider entries.
//!
//! Built-in providers are now **derived from the compiled-in registry snapshot**
//! ([`crate::registry::embedded`]) rather than hand-authored TOMLs: every
//! provider in the snapshot that declares an `auth` scheme (the upstreams and
//! gateways the OSS knows how to talk to) becomes a [`ProviderEntry`]. The one
//! exception is `bitrouter` — the hosted cloud *gateway* — which keeps its own
//! TOML: its id collides with the registry's internal pool entry, and it is the
//! cloud-applier / serves-all-canonical mechanism (an OSS built-in by design).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use bitrouter_sdk::config::ProviderClass;
use bitrouter_sdk::language_model::types::{ApiProtocol, ProtocolList};

use crate::LoadError;
use crate::entry::{AuthScheme, ProtocolMapping, ProviderEntry};
use crate::registry::embedded;
use crate::registry::types::{
    Billing, RegistryAuth, RegistryAuthKind, RegistryKind, RegistryProvider,
};

/// The hosted bitrouter cloud gateway — the sole hand-authored built-in. Its id
/// shadows the registry's pool entry, so it is kept here (not derived from the
/// snapshot) and listed first (the zero-config onboarding hint recommends it).
const BITROUTER_TOML: &str = include_str!("../providers/bitrouter.toml");

static REGISTRY: OnceLock<Vec<ProviderEntry>> = OnceLock::new();

/// Parse + return every built-in entry. Panics only if the compiled-in data is
/// malformed — a build-time invariant caught by `cargo test`, never a user
/// error (the snapshot is registry-validated and drift-checked).
pub fn all() -> &'static [ProviderEntry] {
    REGISTRY
        .get_or_init(|| load_embedded().expect("built-in provider registry must parse"))
        .as_slice()
}

/// Look up a built-in entry by `id`. Returns `None` for unknown ids.
pub fn find(id: &str) -> Option<&'static ProviderEntry> {
    all().iter().find(|e| e.id == id)
}

/// Build the built-in entries: the `bitrouter` cloud gateway (from its TOML)
/// followed by every auth-bearing provider in the embedded registry snapshot.
/// Separated from [`all`] so tests can assert on the `Result`.
pub fn load_embedded() -> Result<Vec<ProviderEntry>, LoadError> {
    let bitrouter: ProviderEntry =
        toml::from_str(BITROUTER_TOML).map_err(|source| LoadError::Parse {
            id: "bitrouter".to_string(),
            source,
        })?;
    let mut out = vec![bitrouter];

    let providers = embedded::providers().map_err(|e| LoadError::Snapshot {
        message: format!("parsing embedded providers.json: {e}"),
    })?;
    for provider in &providers {
        // Only providers the OSS knows how to authenticate become built-ins;
        // the rest are routed via the credential-gated registry merge. The
        // `bitrouter` pool entry (no auth) is excluded — the cloud gateway
        // above owns that id.
        if provider.auth.is_none() || provider.name == "bitrouter" {
            continue;
        }
        let entry = registry_provider_to_entry(provider)?;
        if out.iter().any(|e: &ProviderEntry| e.id == entry.id) {
            return Err(LoadError::DuplicateId { id: entry.id });
        }
        out.push(entry);
    }
    Ok(out)
}

/// Derive a [`ProviderEntry`] (the OSS's compiled-in auth + transport shape)
/// from a registry snapshot provider.
fn registry_provider_to_entry(p: &RegistryProvider) -> Result<ProviderEntry, LoadError> {
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

    #[test]
    fn embedded_registry_parses_cleanly() {
        let entries = load_embedded().expect("embedded TOML files must parse");
        // Bump this when adding a new provider — keeps the test honest about
        // catalog growth.
        assert_eq!(entries.len(), 9);
    }

    #[test]
    fn bitrouter_is_first_for_onboarding_priority() {
        let entries = load_embedded().expect("embedded TOML files must parse");
        assert_eq!(
            entries.first().map(|e| e.id.as_str()),
            Some("bitrouter"),
            "`bitrouter` must lead the catalog so the zero-config hint \
             recommends the hosted gateway first"
        );
    }

    #[test]
    fn bitrouter_parses_with_bearer_env_var() {
        let entry = find("bitrouter").expect("`bitrouter` must be in the catalog");
        assert_eq!(entry.api_base, "https://api.bitrouter.ai/v1");
        assert_eq!(entry.auth.env_var(), Some("BITROUTER_API_KEY"));
        use bitrouter_sdk::language_model::types::ApiProtocol;
        assert_eq!(
            entry.api_protocol.resolve("gpt-4o"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
    }

    #[test]
    fn openai_advertises_chat_and_responses() {
        use bitrouter_sdk::language_model::types::ApiProtocol;
        // OpenAI serves the same models over both Chat Completions and the
        // Responses API at one base URL. Advertising the ordered set lets
        // protocol-native routing honour an inbound Responses request without
        // per-request config, while Chat Completions stays the preferred head
        // (the default for any other inbound protocol).
        let openai = find("openai").unwrap();
        assert_eq!(
            openai.api_protocol.resolve("gpt-5.5"),
            Some(vec![ApiProtocol::ChatCompletions, ApiProtocol::Responses])
        );
    }

    #[test]
    fn looks_up_by_id() {
        assert!(find("bitrouter").is_some());
        assert!(find("openai").is_some());
        assert!(find("openai-codex").is_some());
        assert!(find("anthropic").is_some());
        assert!(find("google").is_some());
        assert!(find("openrouter").is_some());
        assert!(find("github-copilot").is_some());
        assert!(find("opencode-zen").is_some());
        assert!(find("opencode-go").is_some());
        assert!(find("definitely-not-a-provider").is_none());
    }

    #[test]
    fn opencode_zen_per_model_protocols() {
        use bitrouter_sdk::language_model::types::ApiProtocol;
        let zen = find("opencode-zen").unwrap();
        // GPT family → Responses (zen serves them via /responses).
        assert_eq!(
            zen.api_protocol.resolve("opencode/gpt-5.5"),
            Some(vec![ApiProtocol::Responses])
        );
        assert_eq!(
            zen.api_protocol.resolve("opencode/gpt-5.3-codex"),
            Some(vec![ApiProtocol::Responses])
        );
        // Claude family → Messages.
        assert_eq!(
            zen.api_protocol.resolve("opencode/claude-opus-4.7"),
            Some(vec![ApiProtocol::Messages])
        );
        // Gemini family → Google.
        assert_eq!(
            zen.api_protocol.resolve("opencode/gemini-3.1-pro"),
            Some(vec![ApiProtocol::GenerateContent])
        );
        // Everything else (qwen, glm, kimi, minimax, …) → Chat Completions.
        assert_eq!(
            zen.api_protocol.resolve("opencode/qwen3.6-plus"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
        assert_eq!(
            zen.api_protocol.resolve("opencode/minimax-m2.7"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
    }

    #[test]
    fn opencode_go_per_model_protocols() {
        use bitrouter_sdk::language_model::types::ApiProtocol;
        let go = find("opencode-go").unwrap();
        // MiniMax → Messages (go serves MiniMax via /messages).
        assert_eq!(
            go.api_protocol.resolve("opencode-go/minimax-m2.7"),
            Some(vec![ApiProtocol::Messages])
        );
        // Everyone else (glm, kimi, deepseek, mimo, qwen) → Chat Completions.
        assert_eq!(
            go.api_protocol.resolve("opencode-go/glm-5.1"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
        assert_eq!(
            go.api_protocol.resolve("opencode-go/kimi-k2.6"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
        assert_eq!(
            go.api_protocol.resolve("opencode-go/deepseek-v4-pro"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
    }

    #[test]
    fn opencode_zen_and_go_share_one_env_var() {
        // The user opens *one* opencode.ai account; both gateway tiers
        // authenticate with the same `OPENCODE_ZEN_API_KEY`, so a
        // subscriber to Go gets Zen pay-as-you-go billing fall-through
        // (and vice versa) without juggling two creds.
        assert_eq!(
            find("opencode-zen").unwrap().auth.env_var(),
            Some("OPENCODE_ZEN_API_KEY")
        );
        assert_eq!(
            find("opencode-go").unwrap().auth.env_var(),
            Some("OPENCODE_ZEN_API_KEY")
        );
    }

    #[test]
    fn github_copilot_per_model_protocols() {
        use bitrouter_sdk::language_model::types::ApiProtocol;
        let copilot = find("github-copilot").unwrap();
        // Claude family → Messages.
        assert_eq!(
            copilot.api_protocol.resolve("claude-sonnet-4.6"),
            Some(vec![ApiProtocol::Messages])
        );
        // GPT-5-codex → Responses (chat-completions returns 404 in
        // Copilot for these models).
        assert_eq!(
            copilot.api_protocol.resolve("gpt-5.3-codex"),
            Some(vec![ApiProtocol::Responses])
        );
        // Default → Chat Completions.
        assert_eq!(
            copilot.api_protocol.resolve("gpt-4o"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
        assert_eq!(
            copilot.api_protocol.resolve("gemini-2.5-pro"),
            Some(vec![ApiProtocol::ChatCompletions])
        );
    }

    #[test]
    fn github_copilot_oauth_params_survive_mapping() {
        // Regression: github-copilot uses the device-code flow, whose client
        // config lives ONLY in the registry `auth.params` (unlike PKCE
        // providers, whose config is in the OSS oauth registry). If `map_auth`
        // dropped these, `bitrouter login github-copilot` would report no
        // interactive login path. Assert the device-code params are carried.
        let copilot = find("github-copilot").unwrap();
        let AuthScheme::Oauth { params, .. } = &copilot.auth else {
            panic!("github-copilot must use an OAuth scheme");
        };
        assert!(
            params.contains_key("client_id"),
            "device-code login needs auth.params.client_id"
        );
        assert!(
            params.contains_key("device_authorization_endpoint"),
            "device-code login needs auth.params.device_authorization_endpoint"
        );
        assert!(
            params.contains_key("token_endpoint"),
            "device-code login needs auth.params.token_endpoint"
        );
    }

    #[test]
    fn every_entry_has_a_doc_url() {
        for entry in all() {
            assert!(
                entry.doc_url.starts_with("https://"),
                "{} missing https doc_url",
                entry.id
            );
        }
    }

    #[test]
    fn built_in_classes_are_set_for_priority() {
        use bitrouter_sdk::config::ProviderClass;
        // The hosted gateway and the aggregator gateways carry a class so the
        // auto-cascade ranks them; first-party APIs likewise.
        assert_eq!(
            find("bitrouter").unwrap().class,
            Some(ProviderClass::BitrouterCloud)
        );
        assert_eq!(
            find("openai").unwrap().class,
            Some(ProviderClass::FirstPartyApi)
        );
        assert_eq!(
            find("openai-codex").unwrap().class,
            Some(ProviderClass::FirstPartySubscription)
        );
        assert_eq!(
            find("github-copilot").unwrap().class,
            Some(ProviderClass::GatewaySubscription)
        );
        assert_eq!(
            find("openrouter").unwrap().class,
            Some(ProviderClass::GatewaySubscription)
        );
    }
}
