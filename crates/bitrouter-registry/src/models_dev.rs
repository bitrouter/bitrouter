//! Explicit, non-authoritative provider inventory fallback from models.dev.
//! <https://github.com/anomalyco/models.dev#api>
//! <https://github.com/anomalyco/models.dev/blob/dev/packages/core/src/schema-v2.ts>

use crate::{
    document::RegistryError,
    mapping::{Observation, normalize},
};
use registry_json::{Value, json};

/// Registered adapter for a reviewed models.dev provider key.
pub const KIND: &str = "models_dev";
/// Include specialized model types without inferring their execution protocol.
pub const URL: &str = "https://models.dev/api.json?type=all";

/// Validate an anonymous, merge-only fallback declaration.
///
/// # Errors
/// Rejects credential forwarding, custom mappings, and authoritative lifecycle
/// or pricing claims that this third-party catalog cannot establish.
pub fn validate(feed: &Value) -> Result<(), RegistryError> {
    if feed["request"]["url"] != URL
        || feed["request"]["auth"]["kind"] != "none"
        || feed["request"].get("query").is_some()
        || feed["request"].get("headers").is_some_and(|headers| {
            headers
                .as_object()
                .is_none_or(|headers| !headers.is_empty())
        })
        || feed["pagination"]["kind"] != "none"
        || feed.get("response").is_some()
        || feed["provider_id"].as_str().is_none_or(str::is_empty)
        || feed["inventory"]["mode"] != "merge"
        || feed["inventory"]["scope"] != "public"
        || feed["inventory"]["complete"] != false
    {
        return Err(invalid(
            "models.dev requires an explicit provider and anonymous merge-only public catalog",
        ));
    }
    let writes = feed["writes"]
        .as_array()
        .ok_or_else(|| invalid("Missing adapter write authority"))?;
    if writes.len() != 2
        || !writes.iter().any(|value| value == "models")
        || !writes.iter().any(|value| value == "metadata")
    {
        return Err(invalid(
            "models.dev supports only models and descriptive metadata",
        ));
    }
    crate::mapping::validate(&generic_feed(feed))
}

fn generic_feed(feed: &Value) -> Value {
    let mut generic = feed.clone();
    generic["kind"] = json!("v1_models");
    generic["response"] = json!({
        "items_path":"/data",
        "mapping":{"/id":"/provider_model_id","/name":"/name","/description":"/description"},
        "defaults":{"status":"staging","interfaces":[]}
    });
    generic
}

/// Import exact native identities without granting execution or identity binding.
///
/// # Errors
/// A missing provider, malformed row, inconsistent native ID, or unknown source
/// lifecycle rejects the complete feed and retains its previous observation.
pub fn observation(feed: &Value, snapshot: &Value) -> Result<Observation, RegistryError> {
    validate(feed)?;
    let provider = feed["provider_id"]
        .as_str()
        .ok_or_else(|| invalid("Missing models.dev provider key"))?;
    let selected = snapshot
        .get(provider)
        .filter(|value| value.is_object() && value["id"] == provider)
        .ok_or_else(|| invalid("models.dev provider key and provider ID must agree"))?;
    let models = selected
        .get("models")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("Missing models.dev provider model map"))?;
    let mut rows = Vec::new();
    let mut excluded = 0usize;
    for (native, model) in models {
        if native.is_empty() || model["id"].as_str() != Some(native) {
            return Err(invalid("models.dev model key and native ID must agree"));
        }
        match model.get("status") {
            None => {}
            Some(Value::String(status)) if matches!(status.as_str(), "alpha" | "beta") => {}
            Some(Value::String(status)) if status == "deprecated" => {
                excluded += 1;
                continue;
            }
            _ => {
                return Err(invalid(
                    "Unrecognized models.dev lifecycle; update the adapter",
                ));
            }
        }
        let mut row = json!({"id":native});
        for key in ["name", "description"] {
            if let Some(value) = model.get(key).filter(|value| !value.is_null()) {
                row[key] = value.clone();
            }
        }
        rows.push(row);
    }
    let mut observation = normalize(&generic_feed(feed), &[json!({"data":rows})])?;
    observation.excluded = excluded;
    Ok(observation)
}

fn invalid(message: &str) -> RegistryError {
    RegistryError {
        code: "registry.invalid_data",
        path: "/auto_sync/feeds".into(),
        message: message.into(),
        recovery: "Retain previous inventory; repair the source declaration or update the models.dev adapter".into(),
    }
}
