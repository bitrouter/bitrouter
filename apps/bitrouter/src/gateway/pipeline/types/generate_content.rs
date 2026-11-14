//! Retained native input is distinct from its optional foreign-wire projection.
//!
//! This capsule grants no route, credential, attempt or replay authority. Its
//! constructor is private to the service, and dispatch still consumes the
//! current ordered actual-send owner.

use crate::gateway::model::protocol::generate_content::native::stream::{
    Custody, Decoder, Encoder,
};
use std::sync::{Arc, OnceLock};

use bitrouter_ai::{
    conversion::{ConversionEffect, ConversionLocation, ConversionReason, ConversionReport},
    tenancy::{generate_content::OrderedGenerateContentRequestV1, validation::Validate},
    types::{ApiProtocol, Prompt, Usage},
};
use serde_json::Value;

use crate::gateway::{
    error::{BitrouterError, Result},
    model::{
        conversion::ReportAdmission,
        protocol::{InboundAdapter, OutboundAdapter, generate_content::GenerateContentAdapter},
    },
};

struct Original {
    request: OrderedGenerateContentRequestV1,
    defaults: Option<(String, Value)>,
    stream: OnceLock<Arc<Custody>>,
}

/// Native source data, retaining all fields independently from canonical input.
/// External callers cannot manufacture this capsule from JSON or construct one
/// without the service's validated private envelope path.
#[derive(Clone)]
pub struct Input {
    original: Arc<Original>,
    projection: Option<Box<Prompt>>,
    baseline: Option<Box<Prompt>>,
}

/// A validated raw native reply from this invocation's actual paired attempt.
/// Its contents cannot be constructed or replaced through a public DTO.
#[derive(Clone)]
pub struct Response {
    original: Arc<Original>,
    body: Arc<Value>,
    usage: Option<Usage>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeGenerateContentResponse")
            .finish_non_exhaustive()
    }
}

impl Response {
    pub(crate) fn new(input: &Input, body: Value, usage: Option<Usage>) -> Self {
        Self {
            original: input.original.clone(),
            body: Arc::new(body),
            usage,
        }
    }

    pub(crate) fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }
}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeGenerateContentInput")
            .finish_non_exhaustive()
    }
}

impl Input {
    pub(crate) fn response_body(&self, response: &Response) -> Result<Value> {
        if !Arc::ptr_eq(&self.original, &response.original) {
            return Err(BitrouterError::UpstreamUnavailable);
        }
        Ok(response.body.as_ref().clone())
    }
    pub(crate) fn new(
        request: OrderedGenerateContentRequestV1,
        defaults: Option<(String, Value)>,
    ) -> Result<Self> {
        request
            .validate()
            .map_err(|_| BitrouterError::bad_request("invalid native envelope"))?;
        let mut source: serde_json::Map<_, _> = request.request.clone().into_iter().collect();
        source.insert("model".into(), request.model.clone().into());
        source.insert("stream".into(), request.stream.into());
        // A projection may omit qualified wire normalizations, but it must not
        // turn opaque native data into an executable lossy foreign prompt.
        let projection = GenerateContentAdapter
            .parse_request(Value::Object(source))
            .ok()
            .filter(|prompt| {
                GenerateContentAdapter
                    .render_request(prompt)
                    .ok()
                    .is_some_and(|mut rendered| {
                        normalize_omitted_user_roles(&request, &mut rendered);
                        rendered == Value::Object(request.request.clone().into_iter().collect())
                    })
            })
            .map(Box::new);
        Ok(Self {
            original: Arc::new(Original {
                request,
                defaults,
                stream: OnceLock::new(),
            }),
            baseline: projection.clone(),
            projection,
        })
    }

    pub(crate) fn install_native_stream(&self) -> Result<Decoder> {
        let limit = bitrouter_ai::tenancy::generate_content::MAX_REQUEST_BYTES;
        let custody = Custody::new(limit);
        self.original
            .stream
            .set(custody.clone())
            .map_err(|_| BitrouterError::UpstreamUnavailable)?;
        Ok(Decoder::new(custody, limit))
    }

    pub(crate) fn native_stream_encoder(
        &self,
    ) -> Option<Box<dyn crate::gateway::model::protocol::StreamEncoder>> {
        self.original.stream.get().map(|custody| {
            Box::new(Encoder::new(custody.clone()))
                as Box<dyn crate::gateway::model::protocol::StreamEncoder>
        })
    }

    pub(crate) fn native_stream_custody(&self) -> Option<Arc<Custody>> {
        self.original.stream.get().cloned()
    }

    pub(crate) fn text_characters(&self) -> u64 {
        fn content(value: &Value) -> u64 {
            value
                .get("parts")
                .and_then(Value::as_array)
                .map_or(0, |parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .fold(0_u64, |count, text| {
                            count.saturating_add(text.chars().count() as u64)
                        })
                })
        }
        let request = &self.original.request.request;
        let system = request.get("systemInstruction").map_or(0, content);
        request
            .get("contents")
            .and_then(Value::as_array)
            .map_or(system, |contents| {
                contents
                    .iter()
                    .fold(system, |count, value| count.saturating_add(content(value)))
            })
    }

    pub(crate) fn model(&self) -> &str {
        &self.original.request.model
    }

    pub(crate) fn stream(&self) -> bool {
        self.original.request.stream
    }

    pub(crate) fn projection(&self) -> Option<&Prompt> {
        self.projection.as_deref()
    }

    pub(crate) fn projection_mut(&mut self) -> Option<&mut Prompt> {
        self.projection.as_deref_mut()
    }

    pub(crate) fn seal_defaults(&mut self) {
        self.baseline.clone_from(&self.projection);
    }

    pub(crate) fn cache_allowed(&self, enabled: bool) -> bool {
        let defaults = self
            .original
            .defaults
            .as_ref()
            .and_then(|(_, value)| value.get("supplemental"));
        let fallback = defaults
            .and_then(|value| value.get("__google_top_level__"))
            .and_then(|value| value.get("cachedContent"))
            .or_else(|| defaults.and_then(|value| value.get("cachedContent")));
        let alias = self
            .original
            .request
            .request
            .get("cached_content")
            .into_iter()
            .chain(defaults.and_then(|value| value.get("cached_content")))
            .chain(
                defaults
                    .and_then(|value| value.get("__google_top_level__"))
                    .and_then(|value| value.get("cached_content")),
            )
            .any(|value| !value.is_null());
        enabled
            || !alias
                && self
                    .original
                    .request
                    .request
                    .get("cachedContent")
                    .or(fallback)
                    .is_none_or(Value::is_null)
    }

    pub(crate) fn same_original(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.original, &other.original)
    }

    pub(crate) fn unavailable(protocol: &ApiProtocol) -> BitrouterError {
        let mut report = ConversionReport::default();
        report.push_projection(
            protocol,
            ConversionLocation::Operation,
            ConversionReason::InputContentUnrepresentable,
            ConversionEffect::TaskSemantics,
        );
        BitrouterError::Incompatible { report }
    }

    pub(crate) fn render(&self, model: &str, stream: bool) -> Result<Value> {
        if stream != self.stream() || self.projection != self.baseline {
            return Err(Self::unavailable(&ApiProtocol::Custom(
                bitrouter_ai::tenancy::generate_content::PROTOCOL.into(),
            )));
        }
        let mut envelope = self.original.request.clone();
        envelope.model = model.into();
        let mut body = serde_json::to_value(envelope)
            .map_err(|_| BitrouterError::bad_request("invalid native envelope encoding"))?;
        if let Some((key, value)) = &self.original.defaults {
            body.as_object_mut()
                .ok_or(BitrouterError::UpstreamUnavailable)?
                .insert(key.clone(), value.clone());
        }
        Ok(body)
    }
}

// The legacy native parser treats an omitted content role as user. This is the
// sole structural normalization admitted here; opaque metadata must still match.
fn normalize_omitted_user_roles(original: &OrderedGenerateContentRequestV1, rendered: &mut Value) {
    let Some(source) = original.request.get("contents").and_then(Value::as_array) else {
        return;
    };
    let Some(projected) = rendered.get_mut("contents").and_then(Value::as_array_mut) else {
        return;
    };
    if source.len() != projected.len() {
        return;
    }
    for (source, projected) in source.iter().zip(projected) {
        if source.get("role").is_none()
            && projected.get("role").and_then(Value::as_str) == Some("user")
            && let Some(object) = projected.as_object_mut()
        {
            object.remove("role");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;
    fn input(request: Value, defaults: Option<Value>) -> Result<Input> {
        let request = request
            .as_object()
            .ok_or_else(|| BitrouterError::bad_request("fixture object"))?
            .clone()
            .into_iter()
            .collect();
        Input::new(
            OrderedGenerateContentRequestV1 {
                schema_version: 1,
                model: "native/model".into(),
                stream: false,
                request,
            },
            defaults.map(|value| ("bitrouter_generation_defaults_envelope".into(), value)),
        )
    }
    #[test]
    fn native_generate_content_source_allows_only_qualified_role_normalization() -> TestResult {
        let source = json!({"contents":[{"parts":[{"text":"representable"}]}]});
        let input = input(source.clone(), None)?;
        assert!(input.projection().is_some());
        assert_eq!(input.render("provider/model", false)?["request"], source);
        for part in [
            json!({"text":"signed","thoughtSignature":"original"}),
            json!({"text":"","thoughtSignature":"empty-original"}),
            json!({"unknownNativePart":{"opaque":true}}),
        ] {
            let source = json!({"contents":[{"parts":[part]}]});
            let input = self::input(source.clone(), None)?;
            if let Some(prompt) = input.projection() {
                use crate::gateway::model::protocol::chat_completions::ChatCompletionsAdapter;
                assert!(ChatCompletionsAdapter.render_request(prompt).is_err());
            }
            assert_eq!(input.render("provider/model", false)?["request"], source);
        }
        Ok(())
    }
    #[test]
    fn native_generate_content_cache_source_null_masks_only_exact_native_default() -> TestResult {
        let defaults = json!({"schema_version":2,"supplemental":{"cachedContent":"default"},"configuration_defaults":"preserve_absence","canonical":null});
        let missing = input(
            json!({"contents":[{"parts":[{"text":"input"}]}]}),
            Some(defaults.clone()),
        )?;
        assert!(!missing.cache_allowed(false));
        assert!(missing.cache_allowed(true));
        let source = input(
            json!({"contents":[{"parts":[{"text":"input"}]}],"cachedContent":null}),
            Some(defaults),
        )?;
        assert!(source.cache_allowed(false));
        let alias = input(
            json!({"contents":[{"parts":[{"text":"input"}]}],"cachedContent":null,"cached_content":"native-alias"}),
            None,
        )?;
        assert!(!alias.cache_allowed(false));
        Ok(())
    }
}
