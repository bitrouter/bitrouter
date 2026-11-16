use super::*;
pub(super) async fn upstream() -> Result<MockServer> {
    let server = MockServer::start().await;
    for model in ["model-a", "model-b"] {
        // The endpoint reports the model it actually received and original usage.
        // https://platform.openai.com/docs/api-reference/chat/object
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                json!({"model":model}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"experiment-reply", "object":"chat.completion", "created":0,
                "model":model, "choices":[{"index":0,"message":{"role":"assistant",
                "content":"The parser fix is complete."},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6,
                    "prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
                    "completion_tokens_details":{"reasoning_tokens":0}}
            })))
            .mount(&server)
            .await;
    }
    Ok(server)
}
