//! The Chainlink outbound [`Executor`]: submit + poll, then map to canonical
//! results. Streaming is simulated (Chainlink has no streaming): poll to
//! completion, then emit the final text as one or more `TextDelta` parts.

use std::time::Instant;

use async_trait::async_trait;

use bitrouter_sdk::Result;
use bitrouter_sdk::language_model::{
    ApiProtocol, ExecutionResult, Executor, FinishReason, PipelineContext, Prompt, RoutingTarget,
    StreamPart, StreamPartStream,
};

use crate::PROTOCOL;
use crate::client::{ChainlinkClient, PollConfig};
use crate::map::{completed_to_result, prompt_to_request};

/// Outbound executor for the Chainlink Confidential Inference API.
pub struct ChainlinkExecutor {
    http: reqwest::Client,
    poll: PollConfig,
}

impl Default for ChainlinkExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ChainlinkExecutor {
    /// Build an executor with default polling configuration.
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            poll: PollConfig::default(),
        }
    }

    /// The [`ApiProtocol`] this executor is registered under.
    pub fn protocol() -> ApiProtocol {
        ApiProtocol::Custom(PROTOCOL.to_string())
    }

    fn client(&self, target: &RoutingTarget) -> ChainlinkClient {
        ChainlinkClient::new(
            self.http.clone(),
            target.effective_api_base().to_string(),
            target.effective_api_key().to_string(),
            self.poll,
        )
    }
}

#[async_trait]
impl Executor for ChainlinkExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        _ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        let started = Instant::now();
        let client = self.client(target);
        let req = prompt_to_request(&target.service_id, prompt);
        let submitted = client.submit(&req).await?;
        let done = client.poll_until_done(&submitted.id).await?;
        let elapsed = started.elapsed().as_millis() as u64;

        // Chainlink returns unsigned digests, not a signed attestation. Record
        // them as neutral evidence; verification is on-demand (`bitrouter verify`).
        let mut result = completed_to_result(&done);
        crate::map::stash_evidence(&mut result, &done);
        tracing::info!(
            target: "bitrouter_chainlink",
            inference_id = %done.id,
            model = %target.service_id,
            resources = done.resources.len(),
            "chainlink confidential inference completed (unsigned digests recorded)"
        );

        Ok(ExecutionResult {
            provider_id: target.provider_name.clone(),
            model_id: target.service_id.clone(),
            account_label: target.account_label.clone(),
            result,
            latency_ms: elapsed,
            generation_time_ms: elapsed,
        })
    }

    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        _ctx: &PipelineContext,
    ) -> Result<StreamPartStream> {
        let client = self.client(target);
        let req = prompt_to_request(&target.service_id, prompt);
        let submitted = client.submit(&req).await?;
        let done = client.poll_until_done(&submitted.id).await?;

        let result = completed_to_result(&done);
        let text = match result.content.first() {
            Some(bitrouter_sdk::language_model::Content::Text { text, .. }) => text.clone(),
            _ => String::new(),
        };
        let usage = result.usage;

        let out = async_stream::stream! {
            for chunk in chunk_text(&text) {
                yield Ok(StreamPart::TextDelta { text: chunk });
            }
            if let Some(usage) = usage {
                yield Ok(StreamPart::Usage { usage });
            }
            yield Ok(StreamPart::Finish { reason: FinishReason::Stop });
        };
        Ok(Box::pin(out))
    }
}

/// Split a completed output into stream-sized fragments (whitespace-preserving,
/// roughly word-by-word) for simulated streaming.
fn chunk_text(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if ch == ' ' {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_text_roundtrips_byte_exact() {
        let s = "The report identifies three risks.";
        let joined: String = chunk_text(s).concat();
        assert_eq!(joined, s);
        assert!(chunk_text(s).len() > 1);
    }

    #[test]
    fn chunk_text_empty_is_empty() {
        assert!(chunk_text("").is_empty());
    }

    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::language_model::types::AuthScheme;
    use bitrouter_sdk::language_model::{Content, Message, PipelineRequest, Role};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn target(base: String) -> RoutingTarget {
        RoutingTarget {
            provider_name: "chainlink".into(),
            service_id: "gemma4".into(),
            api_base: base,
            api_key: "test-key".into(),
            api_protocol: ChainlinkExecutor::protocol(),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: AuthScheme::Bearer,
        }
    }

    fn prompt() -> Prompt {
        Prompt {
            model: "gemma4".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "hi")],
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn make_ctx(p: &Prompt) -> PipelineContext {
        let req = PipelineRequest::new("gemma4", CallerContext::local(), p.clone());
        PipelineContext::new(req)
    }

    #[tokio::test]
    async fn execute_submits_polls_and_maps() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/inference"))
            .respond_with(
                ResponseTemplate::new(202)
                    .set_body_json(serde_json::json!({ "id": "j", "status": "queued" })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/inference/j"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "j", "status": "completed", "output": "hello",
                "usage": { "prompt_tokens": 2, "completion_tokens": 1 }
            })))
            .mount(&server)
            .await;

        let mut exec = ChainlinkExecutor::new();
        exec.poll = PollConfig {
            interval: std::time::Duration::from_millis(5),
            timeout: std::time::Duration::from_secs(2),
        };
        let p = prompt();
        let ctx = make_ctx(&p);
        let res = exec
            .execute(&target(server.uri()), &p, &ctx)
            .await
            .expect("execute");

        match res.result.content.as_slice() {
            [Content::Text { text, .. }] => assert_eq!(text, "hello"),
            other => panic!("expected text, got {other:?}"),
        }
        assert_eq!(res.model_id, "gemma4");
        assert!(res.result.provider_metadata.contains_key("chainlink"));
    }
}
