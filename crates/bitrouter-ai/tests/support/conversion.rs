//! Shared setup and refusal checks for the conversion integration suites.

use bitrouter_ai::conversion::{ConversionReport, ConversionStage};
use bitrouter_ai::error::ModelError;
use bitrouter_ai::protocol::{
    InboundAdapter, OutboundAdapter, chat_completions::ChatCompletionsAdapter,
    generate_content::GenerateContentAdapter, messages::MessagesAdapter,
    responses::ResponsesAdapter,
};
use bitrouter_ai::types::{ApiProtocol, Prompt};
use serde_json::json;

pub fn prompt() -> bitrouter_ai::error::Result<Prompt> {
    ChatCompletionsAdapter
        .parse_request(json!({"model":"fixture","messages":[{"role":"user","content":"keep"}]}))
}

pub fn adapters() -> [(ApiProtocol, Box<dyn OutboundAdapter>); 4] {
    [
        (
            ApiProtocol::ChatCompletions,
            Box::new(ChatCompletionsAdapter),
        ),
        (ApiProtocol::Responses, Box::new(ResponsesAdapter)),
        (ApiProtocol::Messages, Box::new(MessagesAdapter)),
        (
            ApiProtocol::GenerateContent,
            Box::new(GenerateContentAdapter),
        ),
    ]
}

pub fn require_refusal<T>(
    result: bitrouter_ai::error::Result<T>,
    stage: ConversionStage,
) -> Result<ConversionReport, Box<dyn std::error::Error + Send + Sync>> {
    let Err(ModelError::Incompatible { report }) = result else {
        return Err("loss was silently admitted or lost its typed report".into());
    };
    assert!(!report.issues.is_empty());
    assert!(!format!("{report:?}").contains("secret"));
    assert!(!serde_json::to_string(&report)?.contains("secret"));
    for issue in &report.issues {
        assert_eq!(issue.stage, stage);
    }
    Ok(report)
}

pub fn refusal(
    adapter: &dyn OutboundAdapter,
    source: &Prompt,
) -> Result<ConversionReport, Box<dyn std::error::Error + Send + Sync>> {
    require_refusal(
        adapter.render_request(source),
        ConversionStage::RequestProjection,
    )
}
