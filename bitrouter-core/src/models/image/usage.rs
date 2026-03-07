/// Represents the token usage information for an image model call.
#[derive(Debug, Clone)]
pub struct ImageModelUsage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
}
