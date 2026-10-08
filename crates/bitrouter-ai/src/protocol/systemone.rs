//! TypeSafe System One wire codec over canonical classifier semantics.

use std::collections::{BTreeMap, HashSet};

use serde_json::{Map, Value, json};

use crate::classifier::{
    ClassifierAnswer, ClassifierChoice, ClassifierChoiceProbability, ClassifierChoiceValue,
    ClassifierInput, ClassifierLevel, ClassifierPredicateCriteria, ClassifierQuestion,
    ClassifierRequest, ClassifierResponseFailure, ClassifierResult, ClassifierScoreProbability,
    ClassifierText,
};
use crate::error::{ModelError, Result};
use crate::protocol::Transport;
use crate::protocol::classifier::ClassifierCodec;
use crate::protocol::decisions::{validate_answers, validate_extensions};
use crate::target::ModelTarget;
use crate::types::{ApiProtocol, Usage, UsageAvailability, UsageOrigin};

/// Native System One conversion with request-owned positional correlation.
pub struct SystemOneCodec;

fn invalid(field: &str) -> ModelError {
    ModelError::invalid_request(format!(
        "invalid or unsupported System One field at {field}"
    ))
}

fn closed<'a>(value: &'a Value, allowed: &[&str], field: &str) -> Result<&'a Map<String, Value>> {
    let object = value.as_object().ok_or_else(|| invalid(field))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid(field));
    }
    Ok(object)
}

fn text(value: Value, field: &str) -> Result<ClassifierText> {
    let value: ClassifierText = serde_json::from_value(value).map_err(|_| invalid(field))?;
    if !value.is_valid() {
        return Err(invalid(field));
    }
    Ok(value)
}

fn key(question: &ClassifierQuestion, index: usize) -> String {
    question
        .key()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("q{index}"))
}

fn failure(field: &str, usage: Option<Usage>) -> ModelError {
    let usage = usage.and_then(|usage| {
        let raw = usage.raw.as_deref()?;
        let mut decoded = crate::protocol::decisions::decode_usage(raw)
            .ok()
            .or_else(|| decode_usage(raw))?;
        if serde_json::to_vec(raw).ok()?.len() > 64 * 1024 {
            decoded.raw = Some(Box::new(crate::protocol::decisions::known_usage_fields(
                raw,
                &if decoded.availability.is_some() {
                    ApiProtocol::SystemOne
                } else {
                    ApiProtocol::Decisions
                },
            )));
        }
        Some(decoded)
    });
    ModelError::ClassifierResponse {
        failure: ClassifierResponseFailure {
            message: format!("invalid System One field at {field}"),
            usage: usage.map(Box::new),
        },
    }
}

/// Decode reported totals without fabricating cache or reasoning counters.
pub(crate) fn decode_usage(value: &Value) -> Option<Usage> {
    let input = value.get("input_tokens")?.as_u64()?;
    let output = value.get("output_tokens")?.as_u64()?;
    input.checked_add(output)?;
    Some(Usage {
        prompt_tokens: input,
        completion_tokens: output,
        origin: UsageOrigin::ProviderReported,
        raw: Some(Box::new(value.clone())),
        availability: Some(UsageAvailability {
            cache_read: false,
            cache_write: false,
            reasoning: false,
            reported_total: false,
        }),
        ..Default::default()
    })
}

impl SystemOneCodec {
    /// Parse the native map while preserving keys independently of names.
    pub fn parse_request(body: Value) -> Result<ClassifierRequest> {
        let object = closed(&body, &["model", "state", "questions"], "request")?;
        let model = object
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("model"))?
            .to_owned();
        let state = object
            .get("state")
            .cloned()
            .ok_or_else(|| invalid("state"))?;
        let input = match text(state, "state")? {
            ClassifierText::Text(value) => ClassifierInput::Text(value),
            ClassifierText::Structured(value) => ClassifierInput::Structured(value),
            ClassifierText::Null => return Err(invalid("state")),
        };
        let raw_questions = object
            .get("questions")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("questions"))?;
        let mut questions = Vec::with_capacity(raw_questions.len());
        for (id, raw) in raw_questions {
            let object = closed(raw, &["type", "instructions", "criteria"], "questions")?;
            let instructions = object
                .get("instructions")
                .map(|value| {
                    let value: ClassifierText = serde_json::from_value(value.clone())
                        .map_err(|_| invalid("instructions"))?;
                    if !value.is_valid() && !matches!(value, ClassifierText::Null) {
                        return Err(invalid("instructions"));
                    }
                    Ok(value)
                })
                .transpose()?;
            let mut question = match object.get("type").and_then(Value::as_str) {
                Some("noul") => {
                    let criteria = object
                        .get("criteria")
                        .map(|value| {
                            if value.is_null() {
                                return Ok(None);
                            }
                            closed(value, &["true", "false"], "criteria")?;
                            let result: ClassifierPredicateCriteria =
                                serde_json::from_value(value.clone())
                                    .map_err(|_| invalid("criteria"))?;
                            if [&result.positive, &result.negative]
                                .into_iter()
                                .flatten()
                                .flatten()
                                .any(|value| !value.is_valid())
                            {
                                return Err(invalid("criteria"));
                            }
                            Ok(Some(result))
                        })
                        .transpose()?;
                    ClassifierQuestion::Predicate {
                        instructions,
                        name: None,
                        key: None,
                        criteria,
                    }
                }
                Some("choice") => {
                    let criteria = object
                        .get("criteria")
                        .and_then(Value::as_object)
                        .ok_or_else(|| invalid("criteria"))?;
                    let mut choices = Vec::with_capacity(criteria.len());
                    for (option, description) in criteria {
                        choices.push(ClassifierChoice {
                            value: ClassifierChoiceValue::String(option.clone()),
                            description: if description.is_null() {
                                None
                            } else {
                                Some(text(description.clone(), "criteria")?)
                            },
                        });
                    }
                    ClassifierQuestion::Choice {
                        instructions,
                        choices,
                        name: None,
                        key: None,
                    }
                }
                Some("score") => {
                    let criteria = object
                        .get("criteria")
                        .and_then(Value::as_array)
                        .ok_or_else(|| invalid("criteria"))?;
                    let mut levels = Vec::with_capacity(criteria.len());
                    for raw in criteria {
                        let criteria = text(raw.clone(), "criteria")?;
                        levels.push(match criteria {
                            ClassifierText::Text(label) => ClassifierLevel {
                                label,
                                description: None,
                                criteria: None,
                            },
                            value => ClassifierLevel {
                                label: String::new(),
                                description: None,
                                criteria: Some(value),
                            },
                        });
                    }
                    ClassifierQuestion::Score {
                        instructions,
                        levels,
                        name: None,
                        key: None,
                    }
                }
                _ => return Err(invalid("type")),
            };
            question.set_key(id.clone());
            questions.push(question);
        }
        let request = ClassifierRequest {
            model,
            source_protocol: Some(ApiProtocol::SystemOne),
            input,
            questions,
            safety_identifier: None,
        };
        Self::render_request(&request)?;
        Ok(request)
    }

    /// Render only explicitly supported model-visible shapes.
    pub fn render_request(request: &ClassifierRequest) -> Result<Value> {
        if request.safety_identifier.is_some() {
            return Err(invalid("safety_identifier"));
        }
        let state = match &request.input {
            ClassifierInput::Text(value) => Value::String(value.clone()),
            ClassifierInput::Structured(value) if value.is_object() || value.is_array() => {
                value.clone()
            }
            ClassifierInput::Structured(_) | ClassifierInput::Messages(_) => {
                return Err(invalid("state"));
            }
        };
        if request.questions.is_empty() {
            return Err(invalid("questions.count"));
        }
        let mut questions = Map::new();
        for (index, question) in request.questions.iter().enumerate() {
            let mut object = Map::new();
            if let Some(instructions) = question.instructions() {
                if !instructions.is_valid() && !matches!(instructions, ClassifierText::Null) {
                    return Err(invalid("instructions"));
                }
                object.insert(
                    "instructions".into(),
                    serde_json::to_value(instructions).map_err(|_| invalid("instructions"))?,
                );
            }
            match question {
                ClassifierQuestion::Predicate { criteria, .. } => {
                    object.insert("type".into(), json!("noul"));
                    if let Some(criteria) = criteria {
                        if let Some(criteria) = criteria
                            && [&criteria.positive, &criteria.negative]
                                .into_iter()
                                .flatten()
                                .flatten()
                                .any(|value| !value.is_valid())
                        {
                            return Err(invalid("criteria"));
                        }
                        object.insert(
                            "criteria".into(),
                            serde_json::to_value(criteria).map_err(|_| invalid("criteria"))?,
                        );
                    }
                }
                ClassifierQuestion::Choice { choices, .. } => {
                    if !(1..=255).contains(&choices.len()) {
                        return Err(invalid("criteria.count"));
                    }
                    let mut criteria = Map::new();
                    for choice in choices {
                        let ClassifierChoiceValue::String(value) = &choice.value else {
                            return Err(invalid("criteria.boolean"));
                        };
                        if choice
                            .description
                            .as_ref()
                            .is_some_and(|value| !value.is_valid())
                        {
                            return Err(invalid("criteria"));
                        }
                        if criteria
                            .insert(
                                value.clone(),
                                serde_json::to_value(&choice.description)
                                    .map_err(|_| invalid("criteria"))?,
                            )
                            .is_some()
                        {
                            return Err(invalid("criteria.duplicate"));
                        }
                    }
                    object.insert("type".into(), json!("choice"));
                    object.insert("criteria".into(), Value::Object(criteria));
                }
                ClassifierQuestion::Score { levels, .. } => {
                    if !(1..=10).contains(&levels.len()) {
                        return Err(invalid("criteria.count"));
                    }
                    let mut criteria = Vec::with_capacity(levels.len());
                    for level in levels {
                        if level.description.is_some() {
                            return Err(invalid("criteria.description"));
                        }
                        criteria.push(match &level.criteria {
                            Some(value) if value.is_valid() => {
                                serde_json::to_value(value).map_err(|_| invalid("criteria"))?
                            }
                            Some(_) => return Err(invalid("criteria")),
                            None => Value::String(level.label.clone()),
                        });
                    }
                    object.insert("type".into(), json!("score"));
                    object.insert("criteria".into(), Value::Array(criteria));
                }
            }
            if questions
                .insert(key(question, index), Value::Object(object))
                .is_some()
            {
                return Err(invalid("questions.duplicate"));
            }
        }
        Ok(json!({"model": request.model, "state": state, "questions": questions}))
    }

    /// Parse every answer against its map identity and canonical option values.
    pub fn parse_response(body: Value, request: &ClassifierRequest) -> Result<ClassifierResult> {
        let usage = body.get("usage").and_then(decode_usage);
        let fail = |field: &str| failure(field, usage.clone());
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| fail("model"))?
            .to_owned();
        let raw_answers = body
            .get("answers")
            .and_then(Value::as_object)
            .ok_or_else(|| fail("answers"))?;
        if raw_answers.len() != request.questions.len() {
            return Err(fail("answers.count"));
        }
        let mut answers = Vec::with_capacity(request.questions.len());
        let mut seen = HashSet::new();
        for (index, question) in request.questions.iter().enumerate() {
            let id = key(question, index);
            if !seen.insert(id.clone()) {
                return Err(fail("answers.identity"));
            }
            let raw = raw_answers
                .get(&id)
                .and_then(Value::as_object)
                .ok_or_else(|| fail("answers.identity"))?;
            let name = question.name().map(str::to_owned);
            let confidence = || {
                raw.get("confidence")
                    .and_then(Value::as_f64)
                    .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
                    .ok_or_else(|| fail("confidence"))
            };
            let extras = |known: &[&str]| {
                raw.iter()
                    .filter(|(key, _)| !known.contains(&key.as_str()))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<BTreeMap<_, _>>()
            };
            let answer = match (question, raw.get("type").and_then(Value::as_str)) {
                (ClassifierQuestion::Predicate { .. }, Some("noul")) => {
                    ClassifierAnswer::Predicate {
                        name,
                        probability: raw
                            .get("noul")
                            .and_then(Value::as_f64)
                            .ok_or_else(|| fail("noul"))?,
                        extensions: extras(&["type", "noul"]),
                    }
                }
                (ClassifierQuestion::Choice { .. }, Some("choice")) => {
                    let choice = raw
                        .get("choice")
                        .and_then(Value::as_str)
                        .ok_or_else(|| fail("choice"))?
                        .to_owned();
                    let map = raw
                        .get("probabilities")
                        .and_then(Value::as_object)
                        .ok_or_else(|| fail("probabilities"))?;
                    let mut probabilities = Vec::with_capacity(map.len());
                    for (value, probability) in map {
                        probabilities.push(ClassifierChoiceProbability {
                            value: ClassifierChoiceValue::String(value.clone()),
                            probability: probability
                                .as_f64()
                                .ok_or_else(|| fail("probabilities"))?,
                            extensions: BTreeMap::new(),
                        });
                    }
                    ClassifierAnswer::Choice {
                        name,
                        choice: ClassifierChoiceValue::String(choice),
                        probabilities,
                        confidence: confidence()?,
                        extensions: extras(&["type", "choice", "probabilities", "confidence"]),
                    }
                }
                (ClassifierQuestion::Score { .. }, Some("score")) => {
                    let legend = raw
                        .get("legend")
                        .and_then(Value::as_object)
                        .ok_or_else(|| fail("legend"))?;
                    let map = raw
                        .get("probabilities")
                        .and_then(Value::as_object)
                        .ok_or_else(|| fail("probabilities"))?;
                    if legend.len() != map.len() {
                        return Err(fail("legend.count"));
                    }
                    let mut probabilities = Vec::with_capacity(map.len());
                    for (level, probability) in map {
                        let index = level
                            .parse::<u32>()
                            .map_err(|_| fail("probabilities.level"))?;
                        if level != &index.to_string() {
                            return Err(fail("probabilities.level"));
                        }
                        probabilities.push(ClassifierScoreProbability {
                            value: index,
                            label: text(
                                legend
                                    .get(level)
                                    .cloned()
                                    .ok_or_else(|| fail("legend.level"))?,
                                "legend.level",
                            )
                            .map_err(|_| fail("legend.level"))?,
                            probability: probability
                                .as_f64()
                                .ok_or_else(|| fail("probabilities"))?,
                            extensions: BTreeMap::new(),
                        });
                    }
                    let extensions =
                        extras(&["type", "score", "probabilities", "confidence", "legend"]);

                    ClassifierAnswer::Score {
                        name,
                        score: raw
                            .get("score")
                            .and_then(Value::as_f64)
                            .ok_or_else(|| fail("score"))?,
                        probabilities,
                        confidence: confidence()?,
                        extensions,
                    }
                }
                _ => return Err(fail("answers.type")),
            };
            answers.push(answer);
        }
        validate_answers(&answers, request).map_err(|field| fail(&field))?;
        validate_choices(&answers).map_err(fail)?;
        let result = ClassifierResult {
            model,
            protocol: ApiProtocol::SystemOne,
            answers,
            usage: usage.clone().ok_or_else(|| fail("usage"))?,
            extensions: body
                .as_object()
                .ok_or_else(|| fail("response"))?
                .iter()
                .filter(|(key, _)| !["model", "answers", "usage"].contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        };
        // The native legend is a typed System One field, not an OpenAI extension.
        validate_extensions(&result).map_err(fail)?;
        Ok(result)
    }

    /// Render native output or derive the destination confidence statistic.
    pub fn render_response(
        result: &ClassifierResult,
        request: &ClassifierRequest,
    ) -> Result<Value> {
        let fail = |field: &str| failure(field, Some(result.usage.clone()));
        validate_answers(&result.answers, request).map_err(|field| fail(&field))?;
        validate_choices(&result.answers).map_err(fail)?;
        validate_extensions(result).map_err(fail)?;
        let mut answers = Map::new();
        for (index, (answer, question)) in result.answers.iter().zip(&request.questions).enumerate()
        {
            let native = result.protocol == ApiProtocol::SystemOne;
            let object = match answer {
                ClassifierAnswer::Predicate {
                    probability,
                    extensions,
                    ..
                } => {
                    let mut object: Map<String, Value> = if native {
                        extensions.clone().into_iter().collect()
                    } else {
                        Map::new()
                    };
                    object.insert("type".into(), json!("noul"));
                    object.insert("noul".into(), json!(probability));
                    object
                }
                ClassifierAnswer::Choice {
                    choice,
                    probabilities,
                    confidence,
                    extensions,
                    ..
                } => {
                    let ClassifierChoiceValue::String(choice) = choice else {
                        return Err(fail("choice.boolean"));
                    };
                    let mut distribution = Map::new();
                    for entry in probabilities {
                        let ClassifierChoiceValue::String(value) = &entry.value else {
                            return Err(fail("choice.boolean"));
                        };
                        distribution.insert(value.clone(), json!(entry.probability));
                    }
                    let projected = if native {
                        if !(0.0..=1.0).contains(confidence) {
                            return Err(fail("confidence"));
                        }
                        *confidence
                    } else {
                        choice_confidence(probabilities)?
                    };
                    let mut object: Map<String, Value> = if native {
                        extensions.clone().into_iter().collect()
                    } else {
                        Map::new()
                    };
                    object.insert("type".into(), json!("choice"));
                    object.insert("choice".into(), json!(choice));
                    object.insert("probabilities".into(), Value::Object(distribution));
                    object.insert("confidence".into(), json!(projected));
                    object
                }
                ClassifierAnswer::Score {
                    score,
                    probabilities,
                    confidence,
                    extensions,
                    ..
                } => {
                    let mut distribution = Map::new();
                    let mut legend = Map::new();
                    for entry in probabilities {
                        distribution.insert(entry.value.to_string(), json!(entry.probability));
                        legend.insert(entry.value.to_string(), json!(entry.label));
                    }
                    let projected = if native {
                        if !(0.0..=1.0).contains(confidence) {
                            return Err(fail("confidence"));
                        }
                        *confidence
                    } else {
                        score_confidence(probabilities)?
                    };
                    let mut object: Map<String, Value> = if native {
                        extensions.clone().into_iter().collect()
                    } else {
                        Map::new()
                    };
                    object.insert("type".into(), json!("score"));
                    object.insert("score".into(), json!(score));
                    object.insert("probabilities".into(), Value::Object(distribution));
                    object.insert("legend".into(), Value::Object(legend));
                    object.insert("confidence".into(), json!(projected));
                    object
                }
                ClassifierAnswer::Refusal { .. } => {
                    return Err(fail("answers.refusal_unrepresentable"));
                }
            };
            if answers
                .insert(key(question, index), Value::Object(object))
                .is_some()
            {
                return Err(fail("answers.identity"));
            }
        }
        let usage = if result.protocol == ApiProtocol::SystemOne {
            let raw = result.usage.raw.as_deref().ok_or_else(|| fail("usage"))?;
            if decode_usage(raw).as_ref() != Some(&result.usage) {
                return Err(fail("usage.consistency"));
            }
            raw.clone()
        } else {
            json!({"input_tokens": result.usage.prompt_tokens, "output_tokens": result.usage.completion_tokens})
        };
        let mut envelope: Map<String, Value> = if result.protocol == ApiProtocol::SystemOne {
            result.extensions.clone().into_iter().collect()
        } else {
            Map::new()
        };
        envelope.insert("model".into(), json!(result.model));
        envelope.insert("answers".into(), Value::Object(answers));
        envelope.insert("usage".into(), usage);
        Ok(Value::Object(envelope))
    }
}

fn choice_confidence(probabilities: &[ClassifierChoiceProbability]) -> Result<f64> {
    let count = probabilities.len();
    if count < 2 {
        return Err(invalid("confidence.options"));
    }
    let top = probabilities
        .iter()
        .map(|entry| entry.probability)
        .fold(0.0_f64, f64::max);
    let uniform = 1.0 / count as f64;
    Ok(((top - uniform) / (1.0 - uniform)).clamp(0.0, 1.0))
}

fn score_confidence(probabilities: &[ClassifierScoreProbability]) -> Result<f64> {
    let count = probabilities.len();
    if count < 2 {
        return Err(invalid("confidence.levels"));
    }
    let mode = probabilities
        .iter()
        .max_by(|left, right| {
            left.probability
                .total_cmp(&right.probability)
                .then_with(|| right.value.cmp(&left.value))
        })
        .ok_or_else(|| invalid("confidence.levels"))?
        .value;
    let spread: f64 = probabilities
        .iter()
        .map(|entry| entry.probability * f64::from(entry.value.abs_diff(mode)))
        .sum();
    let middle = (count - 1) as f64 / 2.0;
    let uniform: f64 = (0..count)
        .map(|index| (index as f64 - middle).abs())
        .sum::<f64>()
        / count as f64;
    Ok((1.0 - spread / uniform).clamp(0.0, 1.0))
}

impl ClassifierCodec for SystemOneCodec {
    fn parse_request(&self, body: Value) -> Result<ClassifierRequest> {
        Self::parse_request(body)
    }
    fn render_request(&self, request: &ClassifierRequest) -> Result<Value> {
        Self::render_request(request)
    }
    fn parse_response(&self, body: Value, request: &ClassifierRequest) -> Result<ClassifierResult> {
        Self::parse_response(body, request)
    }
    fn render_response(
        &self,
        result: &ClassifierResult,
        request: &ClassifierRequest,
    ) -> Result<Value> {
        Self::render_response(result, request)
    }
}

/// System One uses a JSON POST with an explicit selected bearer credential.
pub struct SystemOneTransport;

#[async_trait::async_trait]
impl Transport for SystemOneTransport {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::SystemOne
    }
    fn endpoint_url(&self, target: &ModelTarget, _stream: bool) -> String {
        format!("{}/systemone", target.api_base.trim_end_matches('/'))
    }
    async fn authorise(
        &self,
        mut request: reqwest::Request,
        target: &ModelTarget,
    ) -> Result<reqwest::Request> {
        let mut value =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", target.api_key)).map_err(
                |_| ModelError::invalid_credential("invalid System One authorization header"),
            )?;
        value.set_sensitive(true);
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, value);
        Ok(request)
    }
}

fn validate_choices(answers: &[ClassifierAnswer]) -> std::result::Result<(), &'static str> {
    for answer in answers {
        if let ClassifierAnswer::Choice {
            choice,
            probabilities,
            ..
        } = answer
        {
            let selected = probabilities
                .iter()
                .find(|entry| &entry.value == choice)
                .ok_or("choice.identity")?
                .probability;
            if probabilities
                .iter()
                .any(|entry| entry.probability > selected + 0.0001)
            {
                return Err("choice.not_maximal");
            }
        }
    }
    Ok(())
}
