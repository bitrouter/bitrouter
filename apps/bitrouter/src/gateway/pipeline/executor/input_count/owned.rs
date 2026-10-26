//! One actual retained count exchange, under the admitted native execution.
use super::{
    HttpExecutor, PipelineContext, Result, RoutingTarget, count_payload, generation_stream,
    invalid, request_digest, semantic_headers,
};
use crate::{
    gateway::model::protocol::{Transport, responses::ResponsesTransport},
    tenancy::{execution::TenantExecution, receipts::auxiliary::count::Permit},
};
use bitrouter_ai::{
    core::evidence::NativeInputCount,
    tenancy::operation::ErrorCode,
    types::{ApiProtocol, Prompt},
};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

const REQUEST_BYTES: usize = 16 * 1024 * 1024;
const RESPONSE_BYTES: usize = 16 * 1024;

/// Minted only beside the two authenticated, mutually checked wire requests.
/// The private owner comparison survives callback/caller-created DTOs.
pub(crate) struct Binding {
    execution: Arc<TenantExecution>,
    generation: String,
    wire: [u8; 32],
    deadline: tokio::time::Instant,
}
impl Binding {
    pub(crate) fn arm(
        &self,
        execution: &TenantExecution,
        permit: &mut Permit,
        request: &reqwest::Request,
    ) -> std::result::Result<(), ErrorCode> {
        if !std::ptr::eq(execution, self.execution.as_ref()) || wire_digest(request)? != self.wire {
            return Err(ErrorCode::Forbidden);
        }
        if tokio::time::Instant::now() >= self.deadline {
            return Err(ErrorCode::CapacityExceeded);
        }
        permit.arm(self.generation.clone(), self.wire)
    }
}
fn wire_digest(request: &reqwest::Request) -> std::result::Result<[u8; 32], ErrorCode> {
    let mut hash = Sha256::new();
    hash.update(b"bitrouter.tenant.count-wire.v1");
    fn add(hash: &mut Sha256, value: &[u8]) {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    add(&mut hash, request.method().as_str().as_bytes());
    add(&mut hash, request.url().as_str().as_bytes());
    for (name, values) in semantic_headers(request) {
        add(&mut hash, name.as_bytes());
        hash.update((values.len() as u64).to_be_bytes());
        for value in values {
            add(&mut hash, &value);
        }
    }
    let body = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .ok_or(ErrorCode::InvalidRequest)?;
    if body.len() > REQUEST_BYTES {
        return Err(ErrorCode::PayloadTooLarge);
    }
    add(&mut hash, body);
    Ok(hash.finalize().into())
}
fn bounded_json(value: &serde_json::Value) -> Result<()> {
    struct Limit(usize);
    impl std::io::Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("count request bound"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Limit(REQUEST_BYTES), value)
        .map_err(|_| invalid("count request exceeds bound"))
}

impl HttpExecutor {
    pub(in crate::gateway::pipeline::executor) async fn count_owned_input(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
        execution: Arc<TenantExecution>,
    ) -> Result<NativeInputCount> {
        if self.shared_authority
            || super::super::shared::has_admission(ctx)
            || target.api_protocol != ApiProtocol::Responses
            || self.auth_appliers.lookup(&target.provider_name).is_some()
            || ctx
                .extension::<crate::gateway::pipeline::native::NativeManagedRequest>()
                .is_none()
            || ctx
                .extension::<crate::gateway::pipeline::context::ProviderContinuation>()
                .is_some()
            || ctx
                .extension::<crate::gateway::pipeline::context::SuppressProviderContinuation>()
                .is_some()
        {
            return Err(execution.reject(ErrorCode::CapabilityMismatch));
        }
        let mut permit = execution
            .reserve_count(target, prompt)
            .map_err(|code| execution.reject(code))?;
        let owner = execution.receipt_owner()?.clone();
        let deadline = owner
            .deadline
            .min(tokio::time::Instant::now() + Duration::from_secs(30));
        let operation = async {
            let (adapter, selected) = self
                .dispatch
                .lookup(&target.api_protocol)
                .ok_or_else(|| Self::no_dispatch_error(target))?;
            let transport: Arc<dyn Transport> = Arc::new(ResponsesTransport);
            let stream = generation_stream(target, ctx);
            let url = transport.endpoint_url(&target.model_target(), stream);
            let count_url = transport
                .input_token_count_endpoint(&target.model_target())
                .ok_or_else(|| invalid("retained count endpoint unavailable"))?;
            if selected.endpoint_url(&target.model_target(), stream) != url
                || selected
                    .input_token_count_endpoint(&target.model_target())
                    .as_ref()
                    != Some(&count_url)
            {
                return Err(execution.reject(ErrorCode::CapabilityMismatch));
            }
            Self::check_response_format(prompt, adapter, target)?;
            let body =
                self.render_execution_request(adapter.as_ref(), target, prompt, ctx, stream)?;
            if ["previous_response_id", "conversation"]
                .iter()
                .any(|name| body.get(*name).is_some_and(|v| !v.is_null()))
            {
                return Err(execution.reject(ErrorCode::CapabilityMismatch));
            }
            bounded_json(&body)?;
            let expected = self
                .managed_expected_body(&body, target, ctx)?
                .ok_or_else(|| invalid("managed count baseline missing"))?;
            let (client, timeouts) = self.client_for(target, true);
            let mut generation = client.build_request(&url, &body)?;
            super::super::apply_provider_headers(&mut generation, target, ctx, true);
            let mut generation = super::super::tenancy::authenticate_static_exact(
                generation,
                target,
                &transport,
                &execution,
                &permit.identity().credential,
            )
            .await?;
            super::super::apply_provider_headers(&mut generation, target, ctx, false);
            super::super::inject_outbound_request_id(&mut generation, ctx)?;
            let (generation, _) = generation.into_parts();
            let generation_url =
                reqwest::Url::parse(&url).map_err(|_| invalid("invalid generation endpoint"))?;
            if generation.url() != &generation_url || generation.method() != reqwest::Method::POST {
                return Err(execution.reject(ErrorCode::Forbidden));
            }
            let finalized: serde_json::Value = serde_json::from_slice(
                generation
                    .body()
                    .and_then(reqwest::Body::as_bytes)
                    .ok_or_else(|| invalid("count generation body missing"))?,
            )
            .map_err(|_| invalid("count generation body is not JSON"))?;
            adapter
                .validate_managed_body(&expected, &finalized, &target.model_target())
                .map_err(|_| invalid("count generation shaping mismatch"))?;
            let generation_digest = request_digest(&generation, target)?;
            let headers = semantic_headers(&generation);
            let payload = count_payload(&finalized)?;
            bounded_json(&payload)?;
            drop(generation);
            drop(finalized);
            drop(expected);
            drop(body);
            let mut request = client.build_request(&count_url, &payload)?;
            super::super::apply_provider_headers(&mut request, target, ctx, true);
            let mut request = super::super::tenancy::authenticate_static_exact(
                request,
                target,
                &transport,
                &execution,
                &permit.identity().credential,
            )
            .await?;
            super::super::apply_provider_headers(&mut request, target, ctx, false);
            super::super::inject_outbound_request_id(&mut request, ctx)?;
            let (mut request, _) = request.into_parts();
            let expected_url =
                reqwest::Url::parse(&count_url).map_err(|_| invalid("invalid count endpoint"))?;
            let actual: serde_json::Value = serde_json::from_slice(
                request
                    .body()
                    .and_then(reqwest::Body::as_bytes)
                    .ok_or_else(|| invalid("count body missing"))?,
            )
            .map_err(|_| invalid("count body is not JSON"))?;
            if request.method() != reqwest::Method::POST
                || request.url() != &expected_url
                || semantic_headers(&request) != headers
                || actual != payload
            {
                return Err(execution.reject(ErrorCode::Forbidden));
            }
            drop(actual);
            drop(payload);
            *request.timeout_mut() = Some(
                timeouts
                    .total
                    .unwrap_or(Duration::from_secs(30))
                    .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
            );
            let binding = Binding {
                execution: execution.clone(),
                generation: generation_digest.clone(),
                deadline,
                wire: wire_digest(&request).map_err(|code| execution.reject(code))?,
            };
            execution
                .arm_count(target, &binding, &mut permit, &request)
                .map_err(|code| execution.reject(code))?;
            let mut response = client
                .send(request, &owner.cancelled)
                .await
                .map_err(|_| invalid("owned count transport failed"))?;
            permit
                .status(response.status().as_u16())
                .map_err(|code| execution.reject(code))?;
            if !response.status().is_success() {
                // Known rejection is retained without waiting for unsafe diagnostics.
                return Err(invalid("provider rejected owned input count"));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| invalid("owned count response failed"))?
            {
                if chunk.len() > RESPONSE_BYTES.saturating_sub(bytes.len()) {
                    return Err(invalid("owned count response exceeds bound"));
                }
                bytes.extend_from_slice(&chunk);
            }
            let result: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| invalid("invalid count JSON"))?;
            if result.get("object").and_then(serde_json::Value::as_str)
                != Some("response.input_tokens")
            {
                return Err(invalid("invalid count object"));
            }
            let input_tokens = result
                .get("input_tokens")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| invalid("invalid count number"))?;
            // Settle BEFORE public callback/report delivery; later local failure
            // cannot rewrite the actual observed measurement.
            permit
                .counted(input_tokens)
                .map_err(|code| execution.reject(code))?;
            Ok(NativeInputCount::Counted {
                input_tokens,
                request_sha256: generation_digest,
                source: "provider_responses_input_tokens".into(),
            })
        };
        tokio::select! {
            result = operation => result,
            _ = owner.cancelled.cancelled() => Err(execution.reject(ErrorCode::CapacityExceeded)),
            _ = tokio::time::sleep_until(deadline) => Err(execution.reject(ErrorCode::CapacityExceeded)),
        }
    }
}

#[cfg(test)]
impl HttpExecutor {
    /// Isolated fixture client; never changes host defaults or production API.
    pub(crate) fn count_client_for_test(&self, client: reqwest::Client) -> Result<()> {
        let mut clients = self
            .clients
            .write()
            .map_err(|_| invalid("test client lock"))?;
        clients.managed_default_client =
            crate::gateway::model::client::ModelClient::with_http_client(
                clients.default_timeouts.clone(),
                self.dispatch.clone(),
                client,
            );
        Ok(())
    }
}
