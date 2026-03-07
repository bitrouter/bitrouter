/// Tool choice strategies for language models when calling tools during generation.
#[derive(Debug, Clone, Default)]
pub enum LanguageModelToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Tool {
        tool_name: String,
    },
}
