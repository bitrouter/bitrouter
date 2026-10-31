//! Registered OpenRouter full-model-list adapter, separate from arbitrary maps.
//!
//! The provider's own native ID is the canonical vocabulary's exact ID by
//! definition. No prefix/suffix, canonical_slug, or other provider is inferred.
//! <https://openrouter.ai/docs/api/api-reference/models/list-all-models-and-their-properties>
//! <https://openrouter.ai/docs/api/reference/overview>

use crate::{
    document::{RegistryError, canonical_id, integer},
    mapping::{Observation, normalize},
};
use registry_json as serde_json;
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Registered source adapter name; unknown adapters remain non-executable.
pub const KIND: &str = "openrouter_models";
/// Identity evidence and provider listing both require this complete URL.
pub const URL: &str = "https://openrouter.ai/api/v1/models?output_modalities=all";

/// Identify the unsupported batch catalog variant without rewriting its ID.
///
/// This is OpenRouter's exact colon-delimited variant, not a substring or a
/// convention inferred for another provider's native model IDs.
/// <https://openrouter.ai/docs/guides/routing/model-variants/overview>
pub fn is_batch_variant(id: &str) -> bool {
    id.split(':').skip(1).any(|variant| variant == "batch")
}

/// Validate the strict full-list completeness contract and exact unique IDs.
///
/// # Errors
/// Malformed known continuation fields never establish a complete snapshot.
pub fn identities(snapshot: &Value) -> Result<BTreeSet<String>, RegistryError> {
    let rows = snapshot["data"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| invalid("Missing nonempty full model list"))?;
    if snapshot
        .get("total_count")
        .is_some_and(|n| integer(n) != Some(rows.len() as u64))
    {
        return Err(invalid("Identity count does not match full list"));
    }
    if let Some(links) = snapshot.get("links") {
        let links = links
            .as_object()
            .ok_or_else(|| invalid("Invalid identity continuation links"))?;
        if let Some(next) = links.get("next") {
            terminal(next)?;
        }
    }
    for field in ["next_cursor", "next_page_token", "nextPageToken"] {
        if let Some(next) = snapshot.get(field) {
            terminal(next)?;
        }
    }
    if snapshot
        .get("has_more")
        .is_some_and(|v| v.as_bool() != Some(false))
    {
        return Err(invalid(
            "Identity list has more results or invalid has_more",
        ));
    }
    let mut ids = BTreeSet::new();
    for row in rows {
        let id = row["id"]
            .as_str()
            .filter(|s| canonical_id(s))
            .ok_or_else(|| invalid("Malformed identity ID"))?;
        if !ids.insert(id.to_owned()) {
            return Err(invalid("Duplicate identity ID"));
        }
    }
    Ok(ids)
}
fn terminal(value: &Value) -> Result<(), RegistryError> {
    if value.is_null() || value.as_str() == Some("") {
        Ok(())
    } else {
        Err(invalid("Unexpected identity continuation"))
    }
}

/// Validate an explicitly opted-in provider adapter before reading credentials.
///
/// # Errors
/// Rejects narrowed requests, pagination, domain changes, and custom mappings.
pub fn validate(feed: &Value) -> Result<(), RegistryError> {
    if feed["request"]["url"] != URL
        || feed.get("response").is_some()
        || feed["pagination"]["kind"] != "none"
        || feed["request"].get("query").is_some()
    {
        return Err(invalid(
            "OpenRouter adapter requires its exact unfiltered full-list request",
        ));
    }
    let writes = feed["writes"]
        .as_array()
        .ok_or_else(|| invalid("Missing adapter write authority"))?;
    if writes.len() != 2
        || !writes.iter().any(|v| v == "models")
        || !writes.iter().any(|v| v == "metadata")
        || feed["inventory"]["scope"] != "public"
        || feed["inventory"]["complete"] != true
    {
        return Err(invalid(
            "OpenRouter adapter requires models/metadata and complete public inventory",
        ));
    }
    let mut generic = generic_feed(feed);
    generic["kind"] = json!("v1_models");
    crate::mapping::validate(&generic)
}
fn generic_feed(feed: &Value) -> Value {
    let mut value = feed.clone();
    value["kind"] = json!("v1_models");
    value["response"] = json!({"items_path":"/data","mapping":{"/provider_model_id":"/provider_model_id","/canonical_id":"/canonical_id","/name":"/name","/description":"/description","/model_types":"/model_types","/interfaces":"/interfaces"},"defaults":{"status":"active","interfaces":[]}});
    value
}

/// Normalize only explicitly advertised interfaces, excluding batch variants.
/// Other model types retain inventory pending registered endpoint/codec support.
///
/// # Errors
/// Invalid full-list or known metadata rejects the complete observation.
pub fn observation(feed: &Value, snapshot: &Value) -> Result<Observation, RegistryError> {
    validate(feed)?;
    identities(snapshot)?;
    let mut rows = Vec::new();
    let mut retract_interfaces = BTreeSet::new();
    for row in snapshot["data"]
        .as_array()
        .ok_or_else(|| invalid("Missing list"))?
    {
        let id = row["id"]
            .as_str()
            .ok_or_else(|| invalid("Missing model ID"))?;
        // The full identity response stays intact; this is an importer scope
        // restriction, not an identity rewrite or an upstream withdrawal.
        if is_batch_variant(id) {
            continue;
        }
        let mut model =
            json!({"provider_model_id":row["id"],"canonical_id":row["id"],"interfaces":[]});
        for key in ["name", "description"] {
            if let Some(value) = row.get(key).filter(|v| !v.is_null()) {
                if !value.is_string() {
                    return Err(invalid("Malformed descriptive model metadata"));
                }
                model[key] = value.clone();
            }
        }
        let input = modalities(row.pointer("/architecture/input_modalities"))?;
        let output = modalities(row.pointer("/architecture/output_modalities"))?;
        let endpoints = modalities(row.get("endpoints"))?;
        let chat_endpoint = endpoints
            .as_ref()
            .map(|paths| paths.iter().any(|p| p == "/api/v1/chat/completions"));
        if chat_endpoint == Some(false)
            || input
                .as_ref()
                .is_some_and(|values| !values.iter().any(|v| v == "text"))
            || output
                .as_ref()
                .is_some_and(|values| values.iter().map(String::as_str).ne(["text"]))
        {
            retract_interfaces.insert(id);
        }
        if input.is_some() && output.is_some() {
            model["model_types"] = json!([]);
        }
        if let (Some(input), Some(output)) = (input, output)
            && output == ["text"]
            && input.iter().any(|s| s == "text")
        {
            model["model_types"] = json!(["language"]);
            // Modalities describe data, not the endpoint that accepts requests.
            // https://openrouter.ai/docs/api/api-reference/models/list-all-models-and-their-properties
            if chat_endpoint != Some(true) {
                rows.push(model);
                continue;
            }
            let mut interface = json!({"protocol":"chat_completions","operation":"generation","input_modalities":input,"output_modalities":output});
            if let Some(parameters) = row.get("supported_parameters").filter(|v| !v.is_null()) {
                let mut capabilities = Vec::new();
                let parameters = modalities(Some(parameters))?
                    .ok_or_else(|| invalid("Missing supported parameters"))?;
                for (parameter, capability) in [
                    ("tools", "tools"),
                    ("reasoning", "reasoning"),
                    ("structured_outputs", "structured_outputs"),
                    ("logprobs", "logprobs"),
                ] {
                    if parameters.iter().any(|v| v == parameter) {
                        capabilities.push(capability);
                    }
                }
                interface["capabilities"] = json!(capabilities);
            }
            if let Some(limit) = row.get("context_length").filter(|v| !v.is_null()) {
                if integer(limit).is_none_or(|n| n == 0) {
                    return Err(invalid("Malformed model context limit"));
                }
                interface["limits"] = json!({"context_window":limit});
            }
            model["interfaces"] = json!([interface]);
        }
        rows.push(model);
    }
    let mut observation = normalize(&generic_feed(feed), &[json!({"data":rows})])?;
    for model in &mut observation.models {
        // Known unsupported output/endpoints retract only owned interfaces.
        // Missing evidence is unobserved; nonempty interfaces merge sparsely
        // so manually verified sibling protocols remain protected.
        if model.value["provider_model_id"]
            .as_str()
            .is_none_or(|id| !retract_interfaces.contains(id))
            || model.value["interfaces"]
                .as_array()
                .is_some_and(|v| !v.is_empty())
        {
            model.observed.remove("/interfaces");
        }
        for (index, interface) in model.value["interfaces"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            for key in interface
                .as_object()
                .ok_or_else(|| invalid("Invalid normalized interface"))?
                .keys()
            {
                model.observed.insert(format!("/interfaces/{index}/{key}"));
            }
        }
    }
    Ok(observation)
}
fn modalities(value: Option<&Value>) -> Result<Option<Vec<String>>, RegistryError> {
    value
        .filter(|v| !v.is_null())
        .map(|v| {
            let mut seen = BTreeSet::new();
            v.as_array()
                .ok_or_else(|| invalid("Expected a string array"))?
                .iter()
                .map(|v| {
                    let s = v
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| invalid("Expected a nonempty metadata string"))?;
                    if !seen.insert(s) {
                        return Err(invalid("Duplicate metadata value"));
                    }
                    Ok(s.to_owned())
                })
                .collect()
        })
        .transpose()
}
fn invalid(message: &str) -> RegistryError {
    RegistryError{code:"canonical_source_unavailable",path:"/data".into(),message:message.into(),recovery:"Retain previous inventory; update the registered OpenRouter adapter if its API contract changed".into()}
}
