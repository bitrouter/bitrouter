#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Warning {
    #[serde(rename_all = "camelCase")]
    Unsupported {
        feature: String,
        details: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Capability {
        feature: String,
        details: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Other { message: String },
}
