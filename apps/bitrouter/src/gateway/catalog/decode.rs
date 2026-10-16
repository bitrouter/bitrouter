//! Service admission for published and historical catalog payloads.

use bitrouter_ai::catalog::types::{CanonicalModel, RegistryData, RegistryProvider};
use serde::Deserialize;

#[derive(Deserialize)]
struct Payload {
    #[serde(deserialize_with = "deserialize_providers")]
    providers: Vec<RegistryProvider>,
    canonical: Vec<CanonicalModel>,
}

pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<RegistryData, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let payload = Payload::deserialize(deserializer)?;
    Ok(RegistryData {
        providers: payload.providers,
        canonical: payload.canonical,
    })
}

pub(super) fn from_slice(bytes: &[u8]) -> Result<RegistryData, serde_json::Error> {
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let data = deserialize(&mut decoder)?;
    decoder.end()?;
    Ok(data)
}

fn retired_protocol_value(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(name) => {
            crate::gateway::providers::retired::protocol_message(name).is_some()
        }
        serde_json::Value::Array(values) => values.iter().any(retired_protocol_value),
        serde_json::Value::Object(fields) => fields.values().any(retired_protocol_value),
        _ => false,
    }
}

fn deserialize_providers<'de, D>(deserializer: D) -> Result<Vec<RegistryProvider>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    providers_from_values(Vec::<serde_json::Value>::deserialize(deserializer)?)
        .map_err(serde::de::Error::custom)
}

/// Filter retired entries before decoding the current protocol vocabulary.
/// Diagnostics contain counts only; the caller's snapshot bytes are untouched.
pub(crate) fn providers_from_values(
    values: Vec<serde_json::Value>,
) -> Result<Vec<RegistryProvider>, serde_json::Error> {
    let mut providers = Vec::new();
    let mut retired_providers = 0usize;
    let mut retired_models = 0usize;
    for mut value in values {
        let retired = value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| {
                crate::gateway::providers::retired::provider_message(name).is_some()
            })
            || value
                .get("api_protocol")
                .is_some_and(retired_protocol_value);
        if retired {
            retired_providers += 1;
            continue;
        }
        if let Some(models) = value
            .get_mut("models")
            .and_then(serde_json::Value::as_array_mut)
        {
            let before = models.len();
            models.retain(|model| {
                !model
                    .get("api_protocol")
                    .is_some_and(retired_protocol_value)
            });
            retired_models += before - models.len();
            if before > 0 && models.is_empty() {
                retired_providers += 1;
                continue;
            }
        }
        providers.push(serde_json::from_value(value)?);
    }
    if retired_providers > 0 || retired_models > 0 {
        tracing::warn!(
            retired_providers,
            retired_models,
            "retired native-protocol catalog entries quarantined"
        );
    }
    Ok(providers)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    #[test]
    fn old_catalog_quarantines_native_entries_but_keeps_supported_models() -> TestResult {
        let original = json!({"providers":[
        {"name":"google-ai","status":"active","models":[{"api_protocol":"antigravity"}]},
        {"name":"vertex","status":"active","models":[{"api_protocol":"google"}]},
        {"name":"legacy-google","status":"active","api_protocol":[{"*":"google"}]},
        {"name":"mixed","status":"active","models":[
            {"id":"org/native","provider_model_id":"old","api_protocol":"google"},
            {"id":"org/chat","provider_model_id":"chat","api_protocol":"openai"}
        ]}
    ],"canonical":[{"id":"google/historical"},{"id":"org/chat"}]});
        let bytes = serde_json::to_vec(&original)?;
        let parsed = super::from_slice(&bytes)?;
        assert_eq!(parsed.providers.len(), 1);
        assert_eq!(parsed.providers[0].name, "mixed");
        assert_eq!(parsed.providers[0].models.len(), 1);
        assert_eq!(parsed.providers[0].models[0].provider_model_id, "chat");
        assert_eq!(parsed.canonical.len(), 2);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes)?,
            original
        );
        Ok(())
    }
}
