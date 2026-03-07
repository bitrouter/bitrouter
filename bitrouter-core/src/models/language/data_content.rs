#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LanguageModelDataContent {
    Bytes(Vec<u8>),
    String(String),
    Url(String),
}
