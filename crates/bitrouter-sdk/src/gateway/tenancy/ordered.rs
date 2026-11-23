//! Explicit ordered dispatch over retained static candidates, without retry or downgrade.

use bitrouter_ai::{
    protocol::{
        chat_completions::{ChatChunk, ChatRequest, ChatResponse},
        messages::{MessagesEvent, MessagesRequest, MessagesResponse},
        responses::{ResponsesEvent, ResponsesRequest, ResponsesResponse},
    },
    tenancy::{
        generation_defaults::{GenerationDefaultsEnvelope, MAX_DEFAULTS_BYTES},
        identity::Digest,
        native_controls::{HEADER, OrderedNativeControlsCapabilitiesV1, PROVIDER_NATIVE_V1},
        ordered::{MAX_SELECTION_HEADER_BYTES, OrderedDispatchBinding},
        validation::Validate,
    },
};
use reqwest::header::HeaderValue;

use super::{
    Body, Error, GatewayClient, Reply, TenantGateway, bearer_headers, reject_stream_shadow,
};

mod generate_content;

/// One ordered v2 submission with explicit protocol capability attachments.
/// Dropping a reply or stream has the same local-only meaning as strict v1.
pub struct OrderedTenantGateway<'a> {
    inner: TenantGateway<'a>,
    route_id: String,
    generation_defaults_digest: Option<Digest>,
    generate_content: Option<bool>,
    catalog_count: Option<usize>,
}

impl std::fmt::Debug for OrderedTenantGateway<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrderedTenantGateway")
            .finish_non_exhaustive()
    }
}

impl GatewayClient {
    /// Prepare one explicit ordered v2 call from trusted credentials and a binding.
    /// Use `OrderedCapabilities::bind` to respect the observed attempt ceiling.
    /// The server rechecks that ceiling, retained membership and current authority.
    /// This helper never discovers capabilities, truncates selection, or falls back to v1.
    ///
    /// # Errors
    /// Rejects invalid bindings, credentials or oversized encoded selection before I/O.
    pub fn ordered_tenant(
        &self,
        credential: &str,
        binding: &OrderedDispatchBinding,
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
            generate_content: None,
            catalog_count: (binding.selection.admission_mode
                == bitrouter_ai::tenancy::admission::AdmissionMode::Profiled
                && binding.selection.route.owner_kind
                    == bitrouter_ai::tenancy::authority::OwnerKind::OperatorPool
                && binding
                    .selection
                    .candidate_indices
                    .iter()
                    .enumerate()
                    .all(|(index, ordinal)| usize::from(*ordinal) == index))
            .then_some(binding.selection.candidate_indices.len()),
            generation_defaults_digest: binding.selection.generation_defaults_digest.clone(),
            inner: TenantGateway {
                generation_defaults: None,
                catalog_selection: None,
                client: self,
                headers,
            },
            route_id: binding.selection.route.resource_id.to_string(),
        })
    }
}

impl OrderedTenantGateway<'_> {
    /// Attach the exact configured instructions used to prepare this binding.
    /// They are carried in the request body and never forwarded to a provider.
    ///
    /// # Errors
    /// Rejects malformed, competing or differently rooted instructions before I/O.
    pub fn with_catalog_selection(
        mut self,
        selection: &bitrouter_ai::tenancy::catalog::CatalogSelectionRequestV1,
    ) -> Result<Self, Error> {
        selection
            .validate()
            .map_err(|_| Error::Request("invalid configured selection"))?;
        if selection.route_id.as_str() != self.route_id
            || self.inner.catalog_selection.is_some()
            || self.catalog_count != Some(selection.candidates.len())
        {
            return Err(Error::Request("configured selection differs from binding"));
        }
        self.inner.catalog_selection = Some(selection.clone());
        Ok(self)
    }

    fn check_catalog_protocol(
        &self,
        protocol: bitrouter_ai::tenancy::ordered::GenerationProtocol,
    ) -> Result<(), Error> {
        if self
            .inner
            .catalog_selection
            .as_ref()
            .is_some_and(|selection| selection.inbound_protocol != protocol)
        {
            return Err(Error::Request(
                "configured selection protocol differs from request",
            ));
        }
        Ok(())
    }

    /// Opt in to opaque native controls using an explicit matching observation.
    ///
    /// Responses prior-response/conversation and Messages context values remain
    /// provider-owned data. A literal public-handle prefix is never resolved.
    /// Current grants, selected credentials and normal fallback rules still apply.
    /// This does not promise provider-state ownership or publication of a handle.
    ///
    /// # Errors
    /// Rejects disabled, invalid or differently bound discovery before any I/O.
    pub fn with_native_controls(
        mut self,
        capabilities: &OrderedNativeControlsCapabilitiesV1,
    ) -> Result<Self, Error> {
        capabilities
            .validate()
            .map_err(|_| Error::Request("invalid native controls capabilities"))?;
        if !capabilities.enabled
            || [
                (
                    "bitrouter-installation-id",
                    capabilities.installation_id.as_str(),
                ),
                ("bitrouter-instance-id", capabilities.instance_id.as_str()),
                (
                    "bitrouter-capability-revision",
                    capabilities.capability_revision.as_str(),
                ),
            ]
            .iter()
            .any(|(name, expected)| {
                self.inner
                    .headers
                    .get(*name)
                    .is_none_or(|value| value.as_bytes() != expected.as_bytes())
            })
        {
            return Err(Error::Request(
                "native controls discovery does not match dispatch",
            ));
        }
        self.inner
            .headers
            .insert(HEADER, HeaderValue::from_static(PROVIDER_NATIVE_V1));
        Ok(self)
    }

    /// Attach defaults with their bound configuration-layer choice.
    /// Supplemental values merge after caller parsing, retaining their independent
    /// outbound provenance. This never changes routing or continuation authority.
    ///
    /// # Errors
    /// Rejects invalid defaults, a competing attachment or a different bound digest.
    pub fn with_generation_defaults_envelope(
        mut self,
        defaults: GenerationDefaultsEnvelope,
    ) -> Result<Self, Error> {
        let digest = match defaults.digest() {
            Ok(digest) => digest,
            Err(_) => {
                // Invalid programmatic Values may exceed the JSON depth budget.
                // Dispose of their children iteratively rather than recursively
                // dropping the rejected tree on this consuming API's error path.
                let mut pending: Vec<_> = defaults.supplemental.into_values().collect();
                while let Some(value) = pending.pop() {
                    match value {
                        serde_json::Value::Array(values) => pending.extend(values),
                        serde_json::Value::Object(values) => pending.extend(values.into_values()),
                        _ => {}
                    }
                }
                return Err(Error::Request("invalid generation defaults envelope"));
            }
        };
        if self.inner.generation_defaults.is_some()
            || self.generation_defaults_digest.as_ref() != Some(&digest)
        {
            return Err(Error::Request(
                "generation defaults envelope differs from binding or competes with attachment",
            ));
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
        if self.generation_defaults_digest.is_some() != self.inner.generation_defaults.is_some() {
            return Err(Error::Request("missing or competing generation defaults"));
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
        self.check_catalog_protocol(
            bitrouter_ai::tenancy::ordered::GenerationProtocol::ChatCompletions,
        )?;
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
        self.check_catalog_protocol(bitrouter_ai::tenancy::ordered::GenerationProtocol::Messages)?;
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
        self.check_catalog_protocol(bitrouter_ai::tenancy::ordered::GenerationProtocol::Responses)?;
        self.validate_request(&request.model, &request.extra)?;
        self.inner.responses_reply(request).await
    }
}

/// Preserve the original native JSON tokens while adding transport metadata.
pub(super) fn catalog_body(
    request: &[u8],
    selection: &bitrouter_ai::tenancy::catalog::CatalogSelectionRequestV1,
) -> Result<Vec<u8>, Error> {
    use serde::{
        Deserialize,
        de::{MapAccess, Visitor},
    };
    struct Object;
    impl<'de> Visitor<'de> for Object {
        type Value = bool;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a configured request object")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<bool, M::Error> {
            let mut empty = true;
            while let Some(key) = map.next_key::<String>()? {
                if key == bitrouter_ai::tenancy::catalog::BODY_MEMBER {
                    return Err(serde::de::Error::custom("competing configured selection"));
                }
                map.next_value::<serde::de::IgnoredAny>()?;
                empty = false;
            }
            Ok(empty)
        }
    }
    struct RequestObject(bool);
    impl<'de> Deserialize<'de> for RequestObject {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_map(Object).map(Self)
        }
    }
    let RequestObject(empty) = serde_json::from_slice(request)
        .map_err(|_| Error::Request("invalid or competing configured request"))?;
    let end = request
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .filter(|index| request[*index] == b'}')
        .ok_or(Error::Request("configured request must be an object"))?;
    let metadata = serde_json::to_vec(selection)
        .map_err(|_| Error::Request("invalid configured selection encoding"))?;
    let member = bitrouter_ai::tenancy::catalog::BODY_MEMBER;
    let size = end
        .checked_add(metadata.len())
        .and_then(|size| size.checked_add(member.len() + 6))
        .filter(|size| *size <= 16 * 1024 * 1024)
        .ok_or(Error::Request("generation request exceeds body limit"))?;
    let mut body = Vec::with_capacity(size);
    body.extend_from_slice(&request[..end]);
    if !empty {
        body.push(b',');
    }
    body.push(b'"');
    body.extend_from_slice(member.as_bytes());
    body.extend_from_slice(b"\":");
    body.extend_from_slice(&metadata);
    body.push(b'}');
    Ok(body)
}

pub(super) fn envelope_body(
    request: &impl serde::Serialize,
    defaults: &GenerationDefaultsEnvelope,
) -> Result<Vec<u8>, Error> {
    #[derive(serde::Serialize)]
    struct Request<'a, T, D> {
        #[serde(flatten)]
        request: &'a T,
        bitrouter_generation_defaults_envelope: &'a D,
    }
    struct Bounded {
        bytes: Vec<u8>,
        depth: usize,
        string: bool,
        escape: bool,
    }
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_DEFAULTS_BYTES.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("generation request body limit"));
            }
            // Count emitted JSON containers before serialization recurses into
            // their children, including programmatically constructed caller data.
            // Ignore structural characters inside escaped JSON strings; state
            // spans writer chunks. https://www.rfc-editor.org/rfc/rfc8259#section-7
            for byte in bytes {
                if self.string {
                    if self.escape {
                        self.escape = false;
                    } else if *byte == b'\\' {
                        self.escape = true;
                    } else if *byte == b'"' {
                        self.string = false;
                    }
                } else {
                    match byte {
                        b'"' => self.string = true,
                        b'{' | b'[' => {
                            self.depth += 1;
                            if self.depth >= 128 {
                                return Err(std::io::Error::other(
                                    "generation request nesting limit",
                                ));
                            }
                        }
                        b'}' | b']' => self.depth -= 1,
                        _ => {}
                    }
                }
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut body = Bounded {
        bytes: Vec::new(),
        depth: 0,
        string: false,
        escape: false,
    };
    serde_json::to_writer(
        &mut body,
        &Request {
            request,
            bitrouter_generation_defaults_envelope: defaults,
        },
    )
    .map_err(|_| Error::Request("invalid or oversized generation request"))?;
    Ok(body.bytes)
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
    fn envelope_body_bounds_whole_request_depth_bytes_and_escaped_strings()
    -> Result<(), Box<dyn std::error::Error>> {
        use serde_json::{Value, json};
        let defaults = serde_json::from_value(
            json!({"schema_version":2,"supplemental":{},"configuration_defaults":"apply"}),
        )?;
        let mut nested = Value::Null;
        for _ in 0..126 {
            nested = json!([nested]);
        }
        let request = json!({"input":nested,"quoted":"\\\"[{]}\\\""});
        let bytes = super::envelope_body(&request, &defaults)?;
        let decoded: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(decoded["quoted"], request["quoted"]);
        let too_deep = json!({"input":[request["input"].clone()]});
        assert!(super::envelope_body(&too_deep, &defaults).is_err());
        let empty = json!({"input":""});
        let overhead = super::envelope_body(&empty, &defaults)?.len();
        let request = json!({"input":"x".repeat(super::MAX_DEFAULTS_BYTES - overhead)});
        assert_eq!(
            super::envelope_body(&request, &defaults)?.len(),
            super::MAX_DEFAULTS_BYTES
        );
        let mut over = request;
        over["extra"] = Value::Null;
        assert!(super::envelope_body(&over, &defaults).is_err());
        Ok(())
    }

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
