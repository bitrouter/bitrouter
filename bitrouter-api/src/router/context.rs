//! Extracts [`RouteContext`] from API request bodies for content-aware
//! routing. Each protocol has its own extractor because request shapes
//! differ, but the resulting `RouteContext` is protocol-agnostic.

/// Whether a string slice contains a fenced code block marker.
fn has_code_fence(text: &str) -> bool {
    text.contains("```")
}

/// OpenAI Chat Completions context extraction.
pub(crate) mod openai_chat {
    use super::*;
    use bitrouter_core::api::openai::chat::types::{
        ChatCompletionRequest, ChatContentPart, ChatMessageContent,
    };
    use bitrouter_core::routers::content::RouteContext;

    pub fn extract(request: &ChatCompletionRequest) -> RouteContext {
        let mut texts: Vec<String> = Vec::new();
        let mut char_count: usize = 0;
        let mut has_code_blocks = false;
        let turn_count = request.messages.len();
        let has_tools = request.tools.as_ref().is_some_and(|t| !t.is_empty());

        for msg in &request.messages {
            if let Some(content) = &msg.content {
                match content {
                    ChatMessageContent::Text(t) => {
                        char_count += t.len();
                        if !has_code_blocks && has_code_fence(t) {
                            has_code_blocks = true;
                        }
                        texts.push(t.to_lowercase());
                    }
                    ChatMessageContent::Parts(parts) => {
                        for part in parts {
                            if let ChatContentPart::Text { text } = part {
                                char_count += text.len();
                                if !has_code_blocks && has_code_fence(text) {
                                    has_code_blocks = true;
                                }
                                texts.push(text.to_lowercase());
                            }
                        }
                    }
                }
            }
        }

        RouteContext {
            text: texts.join(" "),
            has_code_blocks,
            has_tools,
            turn_count,
            char_count,
        }
    }
}

/// OpenAI Responses API context extraction.
pub(crate) mod openai_responses {
    use super::*;
    use bitrouter_core::api::openai::responses::types::{
        ResponsesInput, ResponsesInputContent, ResponsesInputContentPart, ResponsesInputItem,
        ResponsesRequest,
    };
    use bitrouter_core::routers::content::RouteContext;

    pub fn extract(request: &ResponsesRequest) -> RouteContext {
        let mut texts: Vec<String> = Vec::new();
        let mut char_count: usize = 0;
        let mut has_code_blocks = false;
        let has_tools = request.tools.as_ref().is_some_and(|t| !t.is_empty());

        let turn_count = match &request.input {
            ResponsesInput::Text(t) => {
                char_count += t.len();
                if has_code_fence(t) {
                    has_code_blocks = true;
                }
                texts.push(t.to_lowercase());
                1
            }
            ResponsesInput::Items(items) => {
                let count = items.len();
                for item in items {
                    if let ResponsesInputItem::Message(msg) = item
                        && let Some(content) = &msg.content
                    {
                        match content {
                            ResponsesInputContent::Text(t) => {
                                char_count += t.len();
                                if !has_code_blocks && has_code_fence(t) {
                                    has_code_blocks = true;
                                }
                                texts.push(t.to_lowercase());
                            }
                            ResponsesInputContent::Parts(parts) => {
                                for part in parts {
                                    if let ResponsesInputContentPart::InputText { text } = part {
                                        char_count += text.len();
                                        if !has_code_blocks && has_code_fence(text) {
                                            has_code_blocks = true;
                                        }
                                        texts.push(text.to_lowercase());
                                    }
                                }
                            }
                        }
                    }
                }
                count
            }
        };

        RouteContext {
            text: texts.join(" "),
            has_code_blocks,
            has_tools,
            turn_count,
            char_count,
        }
    }
}

/// Anthropic Messages API context extraction.
pub(crate) mod anthropic_messages {
    use super::*;
    use bitrouter_core::api::anthropic::messages::types::{
        AnthropicContentBlock, AnthropicMessageContent, MessagesRequest,
    };
    use bitrouter_core::routers::content::RouteContext;

    pub fn extract(request: &MessagesRequest) -> RouteContext {
        let mut texts: Vec<String> = Vec::new();
        let mut char_count: usize = 0;
        let mut has_code_blocks = false;
        let turn_count = request.messages.len();
        let has_tools = request.tools.as_ref().is_some_and(|t| !t.is_empty());

        for msg in &request.messages {
            if let Some(content) = &msg.content {
                match content {
                    AnthropicMessageContent::Text(t) => {
                        char_count += t.len();
                        if !has_code_blocks && has_code_fence(t) {
                            has_code_blocks = true;
                        }
                        texts.push(t.to_lowercase());
                    }
                    AnthropicMessageContent::Blocks(blocks) => {
                        for block in blocks {
                            if let AnthropicContentBlock::Text { text } = block {
                                char_count += text.len();
                                if !has_code_blocks && has_code_fence(text) {
                                    has_code_blocks = true;
                                }
                                texts.push(text.to_lowercase());
                            }
                        }
                    }
                }
            }
        }

        RouteContext {
            text: texts.join(" "),
            has_code_blocks,
            has_tools,
            turn_count,
            char_count,
        }
    }
}

/// Google GenerateContent API context extraction.
pub(crate) mod google_generate {
    use super::*;
    use bitrouter_core::api::google::generate_content::types::GenerateContentRequest;
    use bitrouter_core::routers::content::RouteContext;

    pub fn extract(request: &GenerateContentRequest) -> RouteContext {
        let mut texts: Vec<String> = Vec::new();
        let mut char_count: usize = 0;
        let mut has_code_blocks = false;
        let turn_count = request.contents.len();
        let has_tools = request.tools.as_ref().is_some_and(|t| !t.is_empty());

        for content in &request.contents {
            if let Some(parts) = &content.parts {
                for part in parts {
                    if let Some(text) = &part.text {
                        char_count += text.len();
                        if !has_code_blocks && has_code_fence(text) {
                            has_code_blocks = true;
                        }
                        texts.push(text.to_lowercase());
                    }
                }
            }
        }

        RouteContext {
            text: texts.join(" "),
            has_code_blocks,
            has_tools,
            turn_count,
            char_count,
        }
    }
}
