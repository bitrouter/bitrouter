//! Native Decisions JSON codec and transport, separate from generation/SSE.

use std::collections::{BTreeMap, HashSet};

use base64::Engine;
use serde_json::{Map, Value};

use crate::decisions::{
    DecisionAnswer, DecisionChoiceValue, DecisionContent, DecisionInput, DecisionInputPart,
    DecisionQuestion, DecisionRequest, DecisionResponseFailure, DecisionResult,
};
use crate::error::{ModelError, Result};
use crate::protocol::Transport;
use crate::target::ModelTarget;
use crate::types::{ApiProtocol, Usage, UsageOrigin};

/// Rounding tolerance for native probability totals and weighted scores.
const DISTRIBUTION_TOLERANCE: f64 = 0.0001;
/// Combined additive response-field budget; oversized extensions fail delivery.
const EXTENSION_BYTES: usize = 64 * 1024;

/// Bidirectional native JSON conversion with no synthetic generation payload.
pub struct DecisionsCodec;

impl DecisionsCodec {
    /// Parse native evidence/questions, refusing unknown fields without echoing them.
    pub fn parse_request(body: Value) -> Result<DecisionRequest> {
        validate_request_shape(&body)?;
        let request: DecisionRequest =
            serde_json::from_value(body).map_err(|_| request_error("request structure"))?;
        validate_request(&request)?;
        Ok(request)
    }

    /// Encode a validated typed request without changing it.
    pub fn render_request(request: &DecisionRequest) -> Result<Value> {
        validate_request(request)?;
        serde_json::to_value(request).map_err(|_| request_error("request structure"))
    }

    /// Decode a complete response, retaining usable usage before validating answers.
    pub fn parse_response(body: Value, request: &DecisionRequest) -> Result<DecisionResult> {
        let usage = body.get("usage").and_then(|raw| decode_usage(raw).ok());
        let fail = |location: &str| completed_error(location, usage.clone());
        let object = body.as_object().ok_or_else(|| fail("response structure"))?;
        let model = object
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| fail("model"))?
            .to_owned();
        let raw_answers = object
            .get("answers")
            .and_then(Value::as_array)
            .ok_or_else(|| fail("answers"))?;
        if raw_answers
            .iter()
            .any(|answer| answer.get("name").is_none())
        {
            return Err(fail("answers.name"));
        }
        let answers: Vec<DecisionAnswer> =
            serde_json::from_value(Value::Array(raw_answers.clone()))
                .map_err(|_| fail("answers structure"))?;
        validate_answers(&answers, request).map_err(|location| fail(&location))?;
        let extensions = object
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "model" | "answers" | "usage"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let result = DecisionResult {
            model,
            answers,
            usage: usage.clone().ok_or_else(|| fail("usage"))?,
            extensions,
        };
        validate_extensions(&result).map_err(fail)?;
        Ok(result)
    }

    /// Render native results, preserving additive fields and original usage.
    pub fn render_response(result: &DecisionResult, request: &DecisionRequest) -> Result<Value> {
        validate_answers(&result.answers, request)
            .map_err(|location| completed_error(&location, Some(result.usage.clone())))?;
        validate_extensions(result)
            .map_err(|location| completed_error(location, Some(result.usage.clone())))?;
        let mut object: Map<String, Value> = result.extensions.clone().into_iter().collect();
        object.insert("model".into(), Value::String(result.model.clone()));
        object.insert(
            "answers".into(),
            serde_json::to_value(&result.answers)
                .map_err(|_| completed_error("answers structure", Some(result.usage.clone())))?,
        );
        let raw = result
            .usage
            .raw
            .as_deref()
            .ok_or_else(|| completed_error("usage", None))?;
        let decoded_usage = decode_usage(raw).map_err(|_| completed_error("usage", None))?;
        if decoded_usage != result.usage {
            return Err(completed_error("usage consistency", Some(decoded_usage)));
        }
        object.insert("usage".into(), raw.clone());
        Ok(Value::Object(object))
    }
}

/// Native JSON POST transport; it is not registered as a generative adapter.
pub struct DecisionsTransport;

#[async_trait::async_trait]
impl Transport for DecisionsTransport {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::Decisions
    }

    fn endpoint_url(&self, target: &ModelTarget, _stream: bool) -> String {
        format!("{}/decisions", target.api_base.trim_end_matches('/'))
    }

    async fn authorise(
        &self,
        mut request: reqwest::Request,
        target: &ModelTarget,
    ) -> Result<reqwest::Request> {
        let mut value =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", target.api_key)).map_err(
                |_| ModelError::invalid_credential("invalid Decisions authorization header"),
            )?;
        value.set_sensitive(true);
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, value);
        Ok(request)
    }
}

fn request_error(location: &str) -> ModelError {
    ModelError::invalid_request(format!(
        "unsupported or invalid Decisions field at {location}"
    ))
}

fn completed_error(location: &str, usage: Option<Usage>) -> ModelError {
    // Canonical fields supplied by a custom executor are not independent
    // accounting proof. Re-decode valid provider usage before retaining it.
    let usage = usage.and_then(|usage| {
        let raw = usage.raw.as_deref()?;
        let mut decoded = decode_usage(raw).ok()?;
        if usage_extension_bytes(raw).ok()? > EXTENSION_BYTES {
            decoded.raw = Some(Box::new(known_usage_fields(raw)));
        }
        Some(decoded)
    });
    ModelError::DecisionResponse {
        failure: DecisionResponseFailure {
            message: format!("invalid Decisions field at {location}"),
            usage: usage.map(Box::new),
        },
    }
}

fn closed_object<'a>(
    value: &'a Value,
    fields: &[&str],
    location: &str,
) -> Result<&'a Map<String, Value>> {
    let object = value.as_object().ok_or_else(|| request_error(location))?;
    if object.keys().any(|key| !fields.contains(&key.as_str())) {
        return Err(request_error(location));
    }
    Ok(object)
}

fn optional_string(object: &Map<String, Value>, field: &str, location: &str) -> Result<()> {
    if object.get(field).is_some_and(|value| !value.is_string()) {
        return Err(request_error(location));
    }
    Ok(())
}

fn validate_request_shape(body: &Value) -> Result<()> {
    let object = closed_object(
        body,
        &["model", "input", "questions", "safety_identifier"],
        "request",
    )?;
    if let Some(messages) = object.get("input").and_then(Value::as_array) {
        for (index, message) in messages.iter().enumerate() {
            let location = format!("input[{index}]");
            let object = closed_object(message, &["role", "content", "type"], &location)?;
            if object.get("role").and_then(Value::as_str) != Some("user")
                || object
                    .get("type")
                    .is_some_and(|value| value.as_str() != Some("message"))
            {
                return Err(request_error(&location));
            }
            if let Some(parts) = object.get("content").and_then(Value::as_array) {
                for (part_index, part) in parts.iter().enumerate() {
                    let location = format!("input[{index}].content[{part_index}]");
                    let fields: &[&str] = match part.get("type").and_then(Value::as_str) {
                        Some("input_text") => &["type", "text"],
                        Some("input_image") => &["type", "image_url", "detail"],
                        _ => return Err(request_error(&location)),
                    };
                    closed_object(part, fields, &location)?;
                }
            }
        }
    }
    let questions = object
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| request_error("questions"))?;
    for (index, question) in questions.iter().enumerate() {
        let location = format!("questions[{index}]");
        let fields: &[&str] = match question.get("type").and_then(Value::as_str) {
            Some("predicate") => &["type", "instructions", "name"],
            Some("choice") => &["type", "instructions", "name", "choices"],
            Some("score") => &["type", "instructions", "name", "levels"],
            _ => return Err(request_error(&location)),
        };
        let object = closed_object(question, fields, &location)?;
        optional_string(object, "name", &format!("{location}.name"))?;
        for (field, keys) in [
            ("choices", ["value", "description"]),
            ("levels", ["label", "description"]),
        ] {
            if let Some(values) = object.get(field).and_then(Value::as_array) {
                for (item, value) in values.iter().enumerate() {
                    let location = format!("questions[{index}].{field}[{item}]");
                    let object = closed_object(value, &keys, &location)?;
                    optional_string(object, "description", &format!("{location}.description"))?;
                }
            }
        }
    }
    Ok(())
}

fn text_limit(text: &str, max: usize, location: &str) -> Result<()> {
    if text.chars().take(max + 1).count() > max {
        return Err(request_error(location));
    }
    Ok(())
}

fn validate_request(request: &DecisionRequest) -> Result<()> {
    text_limit(&request.model, 1_048_576, "model")?;
    if let Some(Some(identifier)) = &request.safety_identifier {
        text_limit(identifier, 128, "safety_identifier")?;
    }
    let mut images = 0_usize;
    match &request.input {
        DecisionInput::Text(text) => text_limit(text, 10_485_760, "input")?,
        DecisionInput::Messages(messages) => {
            for (index, message) in messages.iter().enumerate() {
                match &message.content {
                    DecisionContent::Text(text) => {
                        text_limit(text, 10_485_760, &format!("input[{index}].content"))?
                    }
                    DecisionContent::Parts(parts) => {
                        for (part_index, part) in parts.iter().enumerate() {
                            let location = format!("input[{index}].content[{part_index}]");
                            match part {
                                DecisionInputPart::InputText { text } => {
                                    text_limit(text, 10_485_760, &location)?
                                }
                                DecisionInputPart::InputImage { image_url, .. } => {
                                    images = images.saturating_add(1);
                                    if images > 128 || image_url.len() > 1_073_741_824 {
                                        return Err(request_error("input.images"));
                                    }
                                    let (metadata, bytes) = image_url
                                        .split_once(',')
                                        .ok_or_else(|| request_error(&location))?;
                                    if !metadata.starts_with("data:image/")
                                        || !metadata.ends_with(";base64")
                                        || bytes.is_empty()
                                        || base64::engine::general_purpose::STANDARD
                                            .decode(bytes)
                                            .is_err()
                                    {
                                        return Err(request_error(&location));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if images > 128 {
        return Err(request_error("input.images"));
    }
    for (index, question) in request.questions.iter().enumerate() {
        let location = format!("questions[{index}]");
        text_limit(
            question.instructions(),
            1_048_576,
            &format!("{location}.instructions"),
        )?;
        if let Some(name) = question.name() {
            text_limit(name, 1_048_576, &format!("{location}.name"))?;
        }
        match question {
            DecisionQuestion::Predicate { .. } => {}
            DecisionQuestion::Choice { choices, .. } => {
                let mut seen = HashSet::new();
                for choice in choices {
                    if !seen.insert(&choice.value) {
                        return Err(request_error(&format!("{location}.choices")));
                    }
                    if let Some(description) = &choice.description {
                        text_limit(
                            description,
                            1_048_576,
                            &format!("{location}.choices.description"),
                        )?;
                    }
                }
            }
            DecisionQuestion::Score { levels, .. } => {
                for level in levels {
                    text_limit(&level.label, 1_048_576, &format!("{location}.levels.label"))?;
                    if let Some(description) = &level.description {
                        text_limit(
                            description,
                            1_048_576,
                            &format!("{location}.levels.description"),
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn probability(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn validate_answers(
    answers: &[DecisionAnswer],
    request: &DecisionRequest,
) -> std::result::Result<(), String> {
    if answers.len() != request.questions.len() {
        return Err("answers cardinality".into());
    }
    for (index, (answer, question)) in answers.iter().zip(&request.questions).enumerate() {
        let location = format!("answers[{index}]");
        if answer.name() != question.name() {
            return Err(format!("{location}.name"));
        }
        match (answer, question) {
            (DecisionAnswer::Refusal { .. }, _) => {}
            (
                DecisionAnswer::Predicate {
                    probability: value, ..
                },
                DecisionQuestion::Predicate { .. },
            ) if probability(*value) => {}
            (
                DecisionAnswer::Choice {
                    choice,
                    probabilities,
                    confidence,
                    ..
                },
                DecisionQuestion::Choice { choices, .. },
            ) => {
                let mut seen: HashSet<&DecisionChoiceValue> = HashSet::new();
                if !confidence.is_finite()
                    || !choices.iter().any(|option| &option.value == choice)
                    || probabilities.len() != choices.len()
                    || probabilities.iter().any(|entry| {
                        !probability(entry.probability)
                            || !seen.insert(&entry.value)
                            || !choices.iter().any(|option| option.value == entry.value)
                    })
                    || (probabilities
                        .iter()
                        .map(|entry| entry.probability)
                        .sum::<f64>()
                        - 1.0)
                        .abs()
                        > DISTRIBUTION_TOLERANCE
                {
                    return Err(location);
                }
            }
            (
                DecisionAnswer::Score {
                    score,
                    probabilities,
                    confidence,
                    ..
                },
                DecisionQuestion::Score { levels, .. },
            ) => {
                let mut seen = HashSet::new();
                if !score.is_finite()
                    || *score < 0.0
                    || *score > levels.len().saturating_sub(1) as f64
                    || !confidence.is_finite()
                    || probabilities.len() != levels.len()
                    || probabilities.iter().any(|entry| {
                        !probability(entry.probability)
                            || !seen.insert(entry.value)
                            || levels
                                .get(entry.value as usize)
                                .is_none_or(|level| level.label != entry.label)
                    })
                    || (probabilities
                        .iter()
                        .map(|entry| entry.probability)
                        .sum::<f64>()
                        - 1.0)
                        .abs()
                        > DISTRIBUTION_TOLERANCE
                    || (probabilities
                        .iter()
                        .map(|entry| entry.probability * f64::from(entry.value))
                        .sum::<f64>()
                        - score)
                        .abs()
                        > DISTRIBUTION_TOLERANCE
                {
                    return Err(location);
                }
            }
            _ => return Err(location),
        }
    }
    Ok(())
}

fn decode_usage(raw: &Value) -> std::result::Result<Usage, ()> {
    let input = raw.get("input_tokens").and_then(Value::as_u64).ok_or(())?;
    let output = raw.get("output_tokens").and_then(Value::as_u64).ok_or(())?;
    let total = raw.get("total_tokens").and_then(Value::as_u64).ok_or(())?;
    if input.checked_add(output) != Some(total) {
        return Err(());
    }
    let input_details = raw.get("input_tokens_details").ok_or(())?;
    let output_details = raw.get("output_tokens_details").ok_or(())?;
    let usage = Usage {
        prompt_tokens: input,
        completion_tokens: output,
        cache_read_tokens: input_details
            .get("cached_tokens")
            .and_then(Value::as_u64)
            .ok_or(())?,
        cache_write_tokens: input_details
            .get("cache_write_tokens")
            .and_then(Value::as_u64)
            .ok_or(())?,
        reasoning_tokens: output_details
            .get("reasoning_tokens")
            .and_then(Value::as_u64)
            .ok_or(())?,
        origin: UsageOrigin::ProviderReported,
        raw: Some(Box::new(raw.clone())),
        ..Default::default()
    };
    usage.normalized_buckets().map_err(|_| ())?;
    Ok(usage)
}

fn validate_extensions(result: &DecisionResult) -> std::result::Result<(), &'static str> {
    let mut size = serde_json::to_vec(&result.extensions)
        .map_err(|_| "extensions")?
        .len();
    if let Some(raw) = result.usage.raw.as_deref() {
        size = size.saturating_add(usage_extension_bytes(raw)?);
    }
    if result
        .extensions
        .keys()
        .any(|key| matches!(key.as_str(), "model" | "answers" | "usage"))
    {
        return Err("extensions");
    }
    for answer in &result.answers {
        let (extensions, reserved): (&BTreeMap<String, Value>, &[&str]) = match answer {
            DecisionAnswer::Predicate { extensions, .. } => {
                (extensions, &["type", "name", "probability"])
            }
            DecisionAnswer::Choice {
                extensions,
                probabilities,
                ..
            } => {
                for entry in probabilities {
                    if entry
                        .extensions
                        .keys()
                        .any(|key| matches!(key.as_str(), "value" | "probability"))
                    {
                        return Err("extensions");
                    }
                    size = size.saturating_add(
                        serde_json::to_vec(&entry.extensions)
                            .map_err(|_| "extensions")?
                            .len(),
                    );
                }
                (
                    extensions,
                    &["type", "name", "choice", "probabilities", "confidence"],
                )
            }
            DecisionAnswer::Score {
                extensions,
                probabilities,
                ..
            } => {
                for entry in probabilities {
                    if entry
                        .extensions
                        .keys()
                        .any(|key| matches!(key.as_str(), "value" | "label" | "probability"))
                    {
                        return Err("extensions");
                    }
                    size = size.saturating_add(
                        serde_json::to_vec(&entry.extensions)
                            .map_err(|_| "extensions")?
                            .len(),
                    );
                }
                (
                    extensions,
                    &["type", "name", "score", "probabilities", "confidence"],
                )
            }
            DecisionAnswer::Refusal { extensions, .. } => (extensions, &["type", "name"]),
        };
        if extensions
            .keys()
            .any(|key| reserved.contains(&key.as_str()))
        {
            return Err("extensions");
        }
        size = size.saturating_add(
            serde_json::to_vec(extensions)
                .map_err(|_| "extensions")?
                .len(),
        );
    }
    if size > EXTENSION_BYTES {
        return Err("extensions size");
    }
    Ok(())
}

fn usage_extension_bytes(raw: &Value) -> std::result::Result<usize, &'static str> {
    fn extra_bytes(value: &Value, known: &[&str]) -> std::result::Result<usize, &'static str> {
        let fields = value.as_object().ok_or("usage extensions")?;
        let extras = fields
            .iter()
            .filter(|(key, _)| !known.contains(&key.as_str()))
            .collect::<BTreeMap<_, _>>();
        serde_json::to_vec(&extras)
            .map(|bytes| bytes.len())
            .map_err(|_| "usage extensions")
    }
    let mut bytes = extra_bytes(
        raw,
        &[
            "input_tokens",
            "output_tokens",
            "total_tokens",
            "input_tokens_details",
            "output_tokens_details",
        ],
    )?;
    for (field, known) in [
        (
            "input_tokens_details",
            &["cached_tokens", "cache_write_tokens"][..],
        ),
        ("output_tokens_details", &["reasoning_tokens"][..]),
    ] {
        bytes = bytes.saturating_add(extra_bytes(
            raw.get(field).ok_or("usage extensions")?,
            known,
        )?);
    }
    Ok(bytes)
}

/// Invalid oversized additive metadata is not retained as unbounded evidence;
/// the original required counters remain available on the completed failure.
fn known_usage_fields(raw: &Value) -> Value {
    let mut fields = Map::new();
    for key in ["input_tokens", "output_tokens", "total_tokens"] {
        if let Some(value) = raw.get(key) {
            fields.insert(key.into(), value.clone());
        }
    }
    for (field, known) in [
        (
            "input_tokens_details",
            &["cached_tokens", "cache_write_tokens"][..],
        ),
        ("output_tokens_details", &["reasoning_tokens"][..]),
    ] {
        let mut details = Map::new();
        for key in known {
            if let Some(value) = raw.get(field).and_then(|details| details.get(*key)) {
                details.insert((*key).into(), value.clone());
            }
        }
        fields.insert(field.into(), Value::Object(details));
    }
    Value::Object(fields)
}
