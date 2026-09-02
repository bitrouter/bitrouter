use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::context::PipelineContext;
use bitrouter_sdk::language_model::types::{PipelineRequest, Prompt};
use std::task::Poll;

fn context_without_disconnect_signal() -> PipelineContext {
    let prompt = Prompt {
        model: "test-model".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: Vec::new(),
        tools: Vec::new(),
        params: Default::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    PipelineContext::new(PipelineRequest::new(
        "test-model",
        CallerContext::local(),
        prompt,
    ))
}

#[test]
fn public_disconnect_predicate_reports_connected_without_sdk_signal() {
    let context = context_without_disconnect_signal();

    assert!(!context.client_disconnected());
}

#[tokio::test]
async fn public_disconnect_signal_remains_pending_without_sdk_signal() {
    let context = context_without_disconnect_signal();
    let signal = context.client_disconnected_signal();
    tokio::pin!(signal);

    assert!(matches!(futures::poll!(signal.as_mut()), Poll::Pending));
}
