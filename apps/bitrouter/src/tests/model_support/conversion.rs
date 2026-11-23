//! Shared setup and refusal checks for the conversion integration suites.

use crate::gateway::conversion_report::{ConversionReport, ConversionStage};
use crate::gateway::model::error::ModelError;
use crate::gateway::model::protocol::chat_completions::ChatCompletionsAdapter;
use crate::gateway::model::protocol::messages::MessagesAdapter;
use crate::gateway::model::protocol::responses::ResponsesAdapter;
use crate::gateway::model::protocol::{InboundAdapter, OutboundAdapter};
use bitrouter_ai::types::{ApiProtocol, Prompt};
use serde_json::json;

pub fn prompt() -> crate::gateway::model::error::Result<Prompt> {
    ChatCompletionsAdapter
        .parse_request(json!({"model":"fixture","messages":[{"role":"user","content":"keep"}]}))
}

pub fn adapters() -> [(ApiProtocol, Box<dyn OutboundAdapter>); 3] {
    [
        (
            ApiProtocol::ChatCompletions,
            Box::new(ChatCompletionsAdapter),
        ),
        (ApiProtocol::Responses, Box::new(ResponsesAdapter)),
        (ApiProtocol::Messages, Box::new(MessagesAdapter)),
    ]
}

pub fn require_refusal<T>(
    result: crate::gateway::model::error::Result<T>,
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
