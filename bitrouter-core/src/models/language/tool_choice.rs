#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum LanguageModelToolChoice {
    #[default]
    Auto,
    None,
    Required,
    #[serde(rename_all = "camelCase")]
    Tool {
        tool_name: String,
    },
}
