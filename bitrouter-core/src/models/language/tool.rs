use crate::models::shared::{
    provider::ProviderOptions,
    types::{JsonSchema, JsonValue, Record},
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LanguageModelTool {
    /// type: "function"
    #[serde(rename_all = "camelCase")]
    Function {
        name: String,
        description: Option<String>,
        input_schema: JsonSchema,
        input_examples: Vec<LanguageModelFunctionToolInputExample>,
        strict: Option<bool>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
    /// type: "provider"
    #[serde(rename_all = "camelCase")]
    Provider {
        id: ProviderToolId,
        name: String,
        args: Record<String, JsonValue>,
        /// Provider-specific metadata
        provider_options: Option<ProviderOptions>,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LanguageModelFunctionToolInputExample {
    pub input: JsonValue,
}

#[derive(Debug, Clone)]
pub struct ProviderToolId {
    pub provider_name: String,
    pub tool_id: String,
}

impl serde::Serialize for ProviderToolId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let s = format!("{}.{}", self.provider_name, self.tool_id);
        serializer.serialize_str(&s)
    }
}

impl<'de> serde::Deserialize<'de> for ProviderToolId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let parts: Vec<&str> = s.splitn(2, '.').collect();
        if parts.len() != 2 {
            return Err(serde::de::Error::custom(format!(
                "Invalid provider tool ID format: {}: Follow `<provider_name>.<tool_id>` format",
                s
            )));
        }
        Ok(ProviderToolId {
            provider_name: parts[0].to_string(),
            tool_id: parts[1].to_string(),
        })
    }
}
