//! Explicit operator-paired transport, without public Google route registration.
//!
//! The private envelope separates model/mode authority from native request data.
//! Native Google preparation remains with the authenticated paired endpoint.
//! <https://ai.google.dev/api/generate-content>

use bitrouter_ai::{
    tenancy::{
        generate_content::{BRIDGE_PROTOCOL, MIME, OrderedGenerateContentRequestV1},
        validation::Validate,
    },
    types::{ApiProtocol, GenerateResult, Prompt},
};
use serde_json::Value;

use super::{GenerateContentAdapter, OutboundAdapter, StreamDecoder};
use crate::gateway::model::{
    error::{ModelError, Result},
    protocol::Transport,
    target::ModelTarget,
};

// Canonical conversion is only the representable foreign-provider subset.
// Authenticated native wire custody must not use this projection as its source.
pub(crate) struct Adapter;

impl OutboundAdapter for Adapter {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::Custom(BRIDGE_PROTOCOL.into())
    }

    fn render_request(&self, prompt: &Prompt) -> Result<Value> {
        let Value::Object(request) = GenerateContentAdapter.render_request(prompt)? else {
            return Err(ModelError::invalid_request("invalid native request object"));
        };
        let envelope = OrderedGenerateContentRequestV1 {
            schema_version: 1,
            model: prompt.model.clone(),
            stream: prompt.stream,
            request: request.into_iter().collect(),
        };
        envelope
            .validate()
            .map_err(|_| ModelError::invalid_request("invalid paired native envelope"))?;
        serde_json::to_value(envelope)
            .map_err(|_| ModelError::invalid_request("invalid paired native encoding"))
    }

    fn admission(&self, prompt: &Prompt) -> bitrouter_ai::conversion::ConversionReport {
        GenerateContentAdapter.admission(prompt)
    }

    fn parse_response(&self, body: Value) -> Result<GenerateResult> {
        GenerateContentAdapter.parse_response(body)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        GenerateContentAdapter.stream_decoder()
    }

    fn supports_response_format(&self) -> bool {
        true
    }
}

pub(crate) struct PairedTransport;

#[async_trait::async_trait]
impl Transport for PairedTransport {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::Custom(BRIDGE_PROTOCOL.into())
    }

    fn endpoint_url(&self, target: &ModelTarget, _stream: bool) -> String {
        format!("{}/generate-content", target.api_base.trim_end_matches('/'))
    }

    async fn authorise(
        &self,
        mut request: reqwest::Request,
        target: &ModelTarget,
    ) -> Result<reqwest::Request> {
        let mut value =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", target.api_key))
                .map_err(|_| ModelError::invalid_credential("invalid paired authorization"))?;
        value.set_sensitive(true);
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, value);
        request.headers_mut().insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static(MIME),
        );
        Ok(request)
    }
}

#[cfg(test)]
mod tests;
