//! Registry v1 admission followed by a deliberately lossy UI projection.
//! Complete raw documents, rather than this projection, are retained in caches.

use super::RegistryData;
use bitrouter_ai::catalog::types::{CanonicalModel, RegistryProvider};
use bitrouter_registry::document::{Document, ReaderPolicy, integer};
use registry_json as serde_json;
use serde_json::json;

pub(crate) fn from_slice(bytes: &[u8]) -> anyhow::Result<RegistryData> {
    project(Document::parse(bytes, &ReaderPolicy::default())?)
}

pub(crate) fn project(document: Document) -> anyhow::Result<RegistryData> {
    let mut providers = Vec::new();
    let policy = ReaderPolicy::default();
    for provider in document.providers() {
        let mut value = json!({"name":provider["name"],"status":provider["status"],"models":[]});
        for key in ["display_name", "api_base", "doc_url", "community"] {
            if let Some(v) = provider.get(key) {
                value[key] = v.clone();
            }
        }
        // Unknown executable vocabularies disable only this provider. They
        // remain fully intact in document, including their nested metadata.
        let access = provider["access"].as_str();
        let auth = provider.get("auth");
        let known_auth = auth.is_some_and(|a| {
            matches!(
                a["kind"].as_str(),
                Some("none" | "bearer" | "header" | "oauth" | "native")
            )
        });
        if !policy.supports(provider)
            || !known_auth
            || access
                .is_some_and(|s| !matches!(s, "api_key" | "local_oauth" | "local_pkce" | "private"))
        {
            tracing::warn!(code="registry.unsupported_feature", provider=?provider["name"], observed_auth=?provider["auth"]["kind"], observed_access=?provider["access"], field="/data/providers", recovery="upgrade BitRouter or use a supported registry revision", "registry provider execution contract unavailable");
            value["status"] = json!("unavailable");
        }
        if matches!(
            access,
            Some("api_key" | "local_oauth" | "local_pkce" | "private")
        ) {
            value["access"] = provider["access"].clone();
        }
        if known_auth {
            value["auth"] = provider["auth"].clone();
        }
        value["billing"] = json!(if provider["billing"] == "subscription" {
            "subscription"
        } else {
            "usage_token"
        });
        match provider["kind"].as_str() {
            Some("first_party") => value["kind"] = json!("first_party"),
            Some("aggregator") => value["kind"] = json!("gateway"),
            _ => {}
        }
        let mut models = Vec::new();
        for offer in provider["models"].as_array().into_iter().flatten() {
            for interface in offer["interfaces"].as_array().into_iter().flatten() {
                if !super::execution::supported(interface, &policy) {
                    tracing::warn!(code="registry.unsupported_interface", provider=?provider["name"], model=?offer["provider_model_id"], observed_protocol=?interface["protocol"], observed_operation=?interface["operation"], observed_profile=?interface["protocol_profile"], field="/data/providers/models/interfaces", supported="chat_completions|messages|responses/generation/default; decisions/decisions/default", recovery="upgrade BitRouter or use a supported registry revision", "registry interface retained with execution disabled");
                }
            }
            let native = offer["provider_model_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("admitted native ID missing"))?;
            let id = offer["canonical_id"].as_str().map(str::to_owned).unwrap_or(
                bitrouter_registry::selector::encode(
                    provider["name"].as_str().unwrap_or_default(),
                    native,
                )?,
            );
            let protocols = offer["interfaces"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|i| super::execution::supported(i, &policy))
                .filter_map(|i| match i["protocol"].as_str() {
                    Some("chat_completions") => Some("openai"),
                    Some("messages") => Some("anthropic"),
                    Some("responses") => Some("responses"),
                    Some("decisions") => Some("decisions"),
                    _ => None,
                })
                .collect::<Vec<_>>();
            models.push(json!({"id":id,"provider_model_id":native,"api_protocol":protocols}));
        }
        value["models"] = json!(models);
        providers.push(bitrouter_registry::json::project::<RegistryProvider>(
            &value,
        )?);
    }
    let canonical = document
        .models()
        .iter()
        .map(|m| CanonicalModel {
            id: m["id"].as_str().unwrap_or_default().into(),
            max_input_tokens: integer(&m["max_input_tokens"]),
            max_output_tokens: integer(&m["max_output_tokens"]),
            context_window: integer(&m["context_window"]),
        })
        .collect();
    Ok(RegistryData {
        providers,
        canonical,
        document: Some(document.into_value()),
    })
}
