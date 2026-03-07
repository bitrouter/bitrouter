/// Represents the data content a Language Model generates.
#[derive(Debug, Clone)]
pub enum LanguageModelDataContent {
    Bytes(Vec<u8>),
    String(String),
    Url(String),
}
