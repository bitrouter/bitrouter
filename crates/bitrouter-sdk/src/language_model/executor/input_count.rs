//! Provider input counting on the exact prepared managed request. Counting is
//! explicitly configured; compatible generation endpoints need not implement it.

use sha2::{Digest, Sha256};

use super::{HttpExecutor, RequestBuildInput, apply_provider_continuation};
use crate::error::{BitrouterError, Result};
use crate::language_model::context::PipelineContext;
use crate::language_model::native::{InputTokenCounting, NativeInputCount};
use crate::language_model::types::{ApiProtocol, Prompt, RoutingTarget};

fn invalid(message: &str) -> BitrouterError {
    BitrouterError::bad_request(message)
}

pub(super) fn request_digest(request: &reqwest::Request, target: &RoutingTarget) -> Result<String> {
    let bytes = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .ok_or_else(|| invalid("input counting requires a verifiable request body"))?;
    let body: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| invalid("input counting requires a JSON request"))?;
    let bound = serde_json::to_vec(&(
        &target.provider_name,
        &target.service_id,
        &target.api_protocol,
        request.url().as_str(),
        semantic_headers(request),
        body,
    ))
    .map_err(|_| invalid("input counting request cannot be committed"))?;
    Ok(Sha256::digest(bound)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn semantic_headers(request: &reqwest::Request) -> Vec<(String, Vec<Vec<u8>>)> {
    let mut headers = request
        .headers()
        .keys()
        .filter(|name| {
            !matches!(
                name.as_str(),
                "traceparent"
                    | "tracestate"
                    | "baggage"
                    | "x-request-id"
                    | "x-client-request-id"
                    | "content-length"
            )
        })
        .map(|name| {
            (
                name.as_str().to_owned(),
                request
                    .headers()
                    .get_all(name)
                    .iter()
                    .map(|value| value.as_bytes().to_vec())
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    headers.sort_by(|left, right| left.0.cmp(&right.0));
    headers
}

/// Only documented count parameters are sent. Known generation-only controls
/// do not affect input; unknown extensions fail instead of silently undercounting.
/// https://developers.openai.com/api/reference/typescript/resources/responses/subresources/input_tokens/methods/count
fn count_payload(body: &serde_json::Value) -> Result<serde_json::Value> {
    let object = body
        .as_object()
        .ok_or_else(|| invalid("count input must be an object"))?;
    let mut payload = serde_json::Map::new();
    for (key, value) in object {
        match key.as_str() {
            "conversation" => {
                return Err(invalid(
                    "mutable provider conversations cannot bind an input count",
                ));
            }
            "input"
            | "instructions"
            | "model"
            | "parallel_tool_calls"
            | "personality"
            | "previous_response_id"
            | "reasoning"
            | "text"
            | "tool_choice"
            | "tools" => {
                payload.insert(key.clone(), value.clone());
            }
            "truncation" if value != "disabled" && !value.is_null() => {
                return Err(invalid(
                    "managed input counting forbids automatic truncation",
                ));
            }
            "truncation" => {
                payload.insert(key.clone(), value.clone());
            }
            "max_output_tokens"
            | "temperature"
            | "top_p"
            | "stream"
            | "store"
            | "metadata"
            | "include"
            | "stream_options"
            | "service_tier"
            | "safety_identifier"
            | "user"
            | "prompt_cache_key"
            | "prompt_cache_retention"
            | "background"
            | "max_tool_calls"
            | "top_logprobs" => {}
            _ => {
                return Err(invalid(
                    "input counting does not support a request extension",
                ));
            }
        }
    }
    Ok(serde_json::Value::Object(payload))
}

impl HttpExecutor {
    pub(super) async fn count_managed_input(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<NativeInputCount> {
        if target.api_protocol != ApiProtocol::Responses
            || target.model_constraints.input_token_counting != Some(InputTokenCounting::Responses)
            || self.auth_appliers.lookup(&target.provider_name).is_some()
        {
            return Err(invalid(
                "configured input counting requires Responses with static transport authentication",
            ));
        }
        let (adapter, transport) = self
            .dispatch
            .lookup(&target.api_protocol)
            .ok_or_else(|| Self::no_dispatch_error(target))?;
        let count_url = transport
            .input_token_count_endpoint(target)
            .ok_or_else(|| invalid("transport does not support input counting"))?;
        Self::check_response_format(prompt, adapter, target)?;
        let mut upstream = prompt.clone();
        upstream.model = target.service_id.clone();
        upstream.stream = false;
        let mut body = adapter.render_request_for_target(&upstream, target)?;
        let managed_expected = self.managed_expected_body(&body, target, ctx)?;
        apply_provider_continuation(&mut body, target, ctx)?;
        let url = transport.endpoint_url(target, false);
        let (client, timeouts) = self.client_for(
            target,
            ctx.extension::<crate::language_model::native::NativeManagedRequest>()
                .is_some(),
        );
        let generation = self
            .build_authenticated_request(&RequestBuildInput {
                client: &client,
                timeouts: &timeouts,
                url: &url,
                body: &body,
                managed_expected: managed_expected.as_ref(),
                target,
                transport,
                ctx,
                trace_headers: None,
            })
            .await?;
        let digest = request_digest(&generation, target)?;
        let finalized: serde_json::Value = serde_json::from_slice(
            generation
                .body()
                .and_then(reqwest::Body::as_bytes)
                .ok_or_else(|| invalid("input count body unavailable"))?,
        )
        .map_err(|_| invalid("input count body is not JSON"))?;
        let payload = count_payload(&finalized)?;
        let mut request = client
            .post(&count_url)
            .json(&payload)
            .timeout(
                timeouts
                    .total
                    .unwrap_or(std::time::Duration::from_secs(30))
                    .min(std::time::Duration::from_secs(30)),
            )
            .build()
            .map_err(|_| invalid("cannot construct input counting request"))?;
        super::apply_provider_headers(&mut request, target, ctx, true);
        let mut request = transport.authorise(request, target).await?;
        super::apply_provider_headers(&mut request, target, ctx, false);
        super::inject_outbound_request_id(&mut request, ctx)?;
        let expected_count_url = reqwest::Url::parse(&count_url)
            .map_err(|_| invalid("input counting endpoint is invalid"))?;
        if request.url() != &expected_count_url {
            return Err(invalid("input counting authentication changed endpoint"));
        }
        if semantic_headers(&generation) != semantic_headers(&request) {
            return Err(invalid(
                "input counting authentication or semantic headers changed",
            ));
        }
        // Authentication must not rewrite the input that was counted.
        let sent: serde_json::Value = serde_json::from_slice(
            request
                .body()
                .and_then(reqwest::Body::as_bytes)
                .ok_or_else(|| invalid("input counting authentication removed body"))?,
        )
        .map_err(|_| invalid("input counting authentication changed body"))?;
        if sent != payload {
            return Err(invalid("input counting authentication changed body"));
        }
        let mut response = client
            .execute(request)
            .await
            .map_err(|_| invalid("provider input counting request failed"))?;
        if !response.status().is_success() {
            // Do not persist upstream bodies/URLs that may echo credentials or
            // native continuation IDs into credential-free routing records.
            return Err(BitrouterError::bad_request(format!(
                "provider input counting returned HTTP {}",
                response.status().as_u16(),
            )));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| invalid("provider input counting response failed"))?
        {
            if bytes.len().saturating_add(chunk.len()) > 16 * 1024 {
                return Err(invalid("provider input counting response exceeds bound"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let result: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("provider input counting returned invalid JSON"))?;
        if result.get("object").and_then(serde_json::Value::as_str) != Some("response.input_tokens")
        {
            return Err(invalid(
                "provider input counting returned an invalid object",
            ));
        }
        let input_tokens = result
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| invalid("provider input counting returned an invalid token count"))?;
        Ok(NativeInputCount::Counted {
            input_tokens,
            request_sha256: digest,
            source: "provider_responses_input_tokens".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn count_payload_preserves_input_fields_and_rejects_uncounted_context() -> Result<()> {
        let body = json!({"model":"fixture", "instructions":"required instructions",
            "input":[{"role":"user","content":[{"type":"input_image","image_url":"https://example.invalid/image.png"}]}],
            "tools":[{"type":"function","name":"read","parameters":{"type":"object"}}],
            "text":{"format":{"type":"json_object"}},"reasoning":{"effort":"high"},
            "previous_response_id":"origin-bound-id","max_output_tokens":128,"stream":false});
        let payload = count_payload(&body)?;
        for key in [
            "model",
            "instructions",
            "input",
            "tools",
            "text",
            "reasoning",
            "previous_response_id",
        ] {
            assert_eq!(payload.get(key), body.get(key));
        }
        for (key, value) in [
            ("truncation", json!("auto")),
            ("conversation", json!("mutable-conversation")),
            ("prompt", json!({"id":"template-reference"})),
            ("unknown_context", json!("extra input")),
        ] {
            let mut extended = body.clone();
            extended[key] = value;
            assert!(count_payload(&extended).is_err(), "must reject {key}");
        }
        Ok(())
    }
}
