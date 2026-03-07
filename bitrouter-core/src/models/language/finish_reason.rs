/// Represents the reason why a Language Model finished generating content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageModelFinishReason {
    Stop,
    Length,
    FunctionCall,
    ContentFilter,
    Error,
    Other(String),
}
