//! Explicit ordered dispatch over retained static candidates, without retry or downgrade.

use bitrouter_ai::{
    protocol::{
        chat_completions::{ChatChunk, ChatRequest, ChatResponse},
        messages::{MessagesEvent, MessagesRequest, MessagesResponse},
        responses::{ResponsesEvent, ResponsesRequest, ResponsesResponse},
    },
    tenancy::{
        generation_defaults::{BODY_MEMBER, CanonicalGenerationDefaultsV1},
        identity::Digest,
        ordered::{MAX_SELECTION_HEADER_BYTES, OrderedDispatchBindingV2},
        validation::Validate,
    },
};
use reqwest::header::HeaderValue;

use super::{
    Body, Error, GatewayClient, Reply, TenantGateway, bearer_headers, reject_stream_shadow,
};

/// One ordered v2 submission. Only the three generation protocols are exposed.
/// Dropping a reply or stream has the same local-only meaning as strict v1.
pub struct OrderedTenantGateway<'a> {
    inner: TenantGateway<'a>,
    route_id: String,
    generation_defaults_digest: Option<Digest>,
}

impl std::fmt::Debug for OrderedTenantGateway<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrderedTenantGateway")
            .finish_non_exhaustive()
    }
}

impl GatewayClient {
    /// Prepare one explicit ordered v2 call from trusted credentials and a binding.
    /// Use `OrderedCapabilitiesV2::bind` to respect the observed attempt ceiling.
    /// The server rechecks that ceiling, retained membership and current authority.
    /// This helper never discovers capabilities, truncates selection, or falls back to v1.
    ///
    /// # Errors
    /// Rejects invalid bindings, credentials or oversized encoded selection before I/O.
    pub fn ordered_tenant(
        &self,
        credential: &str,
        binding: &OrderedDispatchBindingV2,
    ) -> Result<OrderedTenantGateway<'_>, Error> {
        binding
            .validate()
            .map_err(|_| Error::Request("invalid ordered dispatch binding"))?;
        let selection = serde_json::to_vec(&binding.selection)
            .map_err(|_| Error::Request("invalid ordered selection encoding"))?;
        let selection = encode_selection(&selection)?;
        let mut headers = bearer_headers(credential)?;
        for (name, value) in [
            (
                "bitrouter-tenancy-version",
                binding.contract_version.as_str(),
            ),
            (
                "bitrouter-installation-id",
                binding.installation_id.as_str(),
            ),
            ("bitrouter-instance-id", binding.instance_id.as_str()),
            (
                "bitrouter-capability-revision",
                binding.capability_revision.as_str(),
            ),
            (
                "bitrouter-execution-revision",
                binding.execution_revision.as_str(),
            ),
            ("bitrouter-correlation-id", binding.correlation_id.as_str()),
            ("bitrouter-receipt-mode", binding.receipt_mode.as_str()),
            ("bitrouter-candidate-selection", selection.as_str()),
        ] {
            headers.insert(
                name,
                HeaderValue::from_str(value)
                    .map_err(|_| Error::Request("invalid ordered dispatch header"))?,
            );
        }
        Ok(OrderedTenantGateway {
            generation_defaults_digest: binding.selection.generation_defaults_digest.clone(),
            inner: TenantGateway {
                generation_defaults: None,
                client: self,
                headers,
            },
            route_id: binding.selection.route.resource_id.to_string(),
        })
    }
}

impl OrderedTenantGateway<'_> {
    /// Attach the canonical defaults already bound to this submission.
    /// Typed caller values win; typed null and an empty stop list permit default
    /// filling. Opaque source extras are never promoted across protocols.
    ///
    /// # Errors
    /// Rejects invalid defaults or a digest different from the frozen binding.
    pub fn with_generation_defaults(
        mut self,
        defaults: CanonicalGenerationDefaultsV1,
    ) -> Result<Self, Error> {
        let digest = defaults
            .digest()
            .map_err(|_| Error::Request("invalid canonical defaults"))?;
        if self.generation_defaults_digest.as_ref() != Some(&digest) {
            return Err(Error::Request("canonical defaults differ from binding"));
        }
        self.inner.generation_defaults = Some(defaults);
        Ok(self)
    }

    /// Replace this call's `anthropic-beta` values without changing its selection.
    ///
    /// Preserves opaque field-value bytes, duplicates and order. An empty slice
    /// removes the header. See [`TenantGateway::with_anthropic_beta`] for the
    /// value validation and upstream forwarding contract.
    #[must_use]
    pub fn with_anthropic_beta(mut self, values: &[HeaderValue]) -> Self {
        self.inner = self.inner.with_anthropic_beta(values);
        self
    }

    fn validate_request(
        &self,
        model: &str,
        extra: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<(), Error> {
        reject_stream_shadow(extra)?;
        if extra.contains_key(BODY_MEMBER)
            || self.generation_defaults_digest.is_some() != self.inner.generation_defaults.is_some()
        {
            return Err(Error::Request("missing or competing canonical defaults"));
        }
        if model != self.route_id || extra.contains_key("model") {
            return Err(Error::Request("ordered route differs from request model"));
        }
        Ok(())
    }

    /// Dispatch ordered Chat Completions JSON or SSE with strict response metadata.
    ///
    /// # Errors
    /// Returns local validation, transport, service or protocol failures without retry.
    pub async fn chat_reply(
        self,
        request: &ChatRequest,
    ) -> Result<Reply<Body<ChatResponse, ChatChunk>>, Error> {
        self.validate_request(&request.model, &request.extra)?;
        self.inner.chat_reply(request).await
    }

    /// Dispatch ordered Messages JSON or SSE with strict response metadata.
    ///
    /// # Errors
    /// Returns local validation, transport, service or protocol failures without retry.
    pub async fn messages_reply(
        self,
        request: &MessagesRequest,
    ) -> Result<Reply<Body<MessagesResponse, MessagesEvent>>, Error> {
        self.validate_request(&request.model, &request.extra)?;
        self.inner.messages_reply(request).await
    }

    /// Dispatch ordered Responses JSON or SSE with strict response metadata.
    ///
    /// # Errors
    /// Returns local validation, transport, service or protocol failures without retry.
    pub async fn responses_reply(
        self,
        request: &ResponsesRequest,
    ) -> Result<Reply<Body<ResponsesResponse, ResponsesEvent>>, Error> {
        self.validate_request(&request.model, &request.extra)?;
        self.inner.responses_reply(request).await
    }
}

fn encode_selection(bytes: &[u8]) -> Result<String, Error> {
    // RFC 4648 URL-safe alphabet, canonical zero pad bits and no '=' padding.
    // https://www.rfc-editor.org/rfc/rfc4648.html#section-5
    // Checking raw length first bounds both arithmetic and output allocation.
    if bytes.len() > MAX_SELECTION_HEADER_BYTES / 4 * 3 {
        return Err(Error::Request(
            "ordered selection header exceeds 4096 bytes",
        ));
    }
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = chunk
            .iter()
            .fold(0_u32, |word, byte| (word << 8) | u32::from(*byte))
            << ((3 - chunk.len()) * 8);
        for index in 0..chunk.len() + 1 {
            output.push(char::from(
                ALPHABET[((word >> (18 - 6 * index)) & 63) as usize],
            ));
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::encode_selection;

    #[test]
    fn rfc4648_unpadded_vectors_and_exact_header_bound() -> Result<(), super::Error> {
        for (bytes, encoded) in [
            (b"".as_slice(), ""),
            (b"f", "Zg"),
            (b"fo", "Zm8"),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg"),
            (b"fooba", "Zm9vYmE"),
            (b"foobar", "Zm9vYmFy"),
            (&[251, 255], "-_8"),
        ] {
            assert_eq!(encode_selection(bytes)?, encoded);
        }
        assert_eq!(encode_selection(&[0; 3072])?.len(), 4096);
        assert!(encode_selection(&[0; 3073]).is_err());
        Ok(())
    }
}
