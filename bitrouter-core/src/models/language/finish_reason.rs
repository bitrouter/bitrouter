#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageModelFinishReason {
    Stop,
    Length,
    FunctionCall,
    ContentFilter,
    Error,
    Other(String),
}

impl serde::Serialize for LanguageModelFinishReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            LanguageModelFinishReason::Stop => serializer.serialize_str("stop"),
            LanguageModelFinishReason::Length => serializer.serialize_str("length"),
            LanguageModelFinishReason::FunctionCall => serializer.serialize_str("function_call"),
            LanguageModelFinishReason::ContentFilter => serializer.serialize_str("content_filter"),
            LanguageModelFinishReason::Error => serializer.serialize_str("error"),
            LanguageModelFinishReason::Other(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> serde::Deserialize<'de> for LanguageModelFinishReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.to_ascii_lowercase().as_str() {
            "stop" => Ok(LanguageModelFinishReason::Stop),
            "length" => Ok(LanguageModelFinishReason::Length),
            "function_call" => Ok(LanguageModelFinishReason::FunctionCall),
            "content_filter" => Ok(LanguageModelFinishReason::ContentFilter),
            "error" => Ok(LanguageModelFinishReason::Error),
            other => Ok(LanguageModelFinishReason::Other(other.to_string())),
        }
    }
}
