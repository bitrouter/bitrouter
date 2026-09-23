//! Native format facet for a pinned Laya checkpoint behind a loopback process.
//!
//! This crate owns only request rendering and response validation. The local
//! Python process owns model loading and inference; the embedding host owns
//! HTTP, credentials, retry policy, and metering.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use bitrouter_sdk::error::{BitrouterError, Result};
use bitrouter_sdk::evaluation::{
    EvaluationAnswer, EvaluationQuestion, EvaluationRequest, EvaluationResult,
    EvaluationRoutingTarget, EvaluationUsage,
};
use bitrouter_sdk::extension::{EvaluationFormatAdapter, EvaluationFormatDescriptor, ExtensionApi};
use serde::Deserialize;
use serde_json::{Value, json};

pub const EXTENSION_ID: &str = "laya";
pub const ADAPTER_ID: &str = "local_system_one";
pub const REVISION: u32 = 1;
/// Exact Hugging Face snapshot loaded by the companion local process.
pub const PROVIDER_MODEL_ID: &str =
    "convaiinnovations/laya-typed-decisions@f9ab0b228f0fc0f14d873dbc99038f135c2da1b2";
pub const MAX_QUESTIONS: usize = 16;
pub const MAX_CHOICE_OPTIONS: usize = 20;
pub const MAX_SCORE_LEVELS: usize = 10;

/// Register the independently versioned Laya wire facet in a trusted host.
pub fn register(api: &mut ExtensionApi) -> Result<()> {
    api.register_evaluation_format(Arc::new(LayaLocalFormat))
}

pub struct LayaLocalFormat;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalResponse {
    model: String,
    answers: BTreeMap<String, EvaluationAnswer>,
    usage: EvaluationUsage,
}

impl EvaluationFormatAdapter for LayaLocalFormat {
    fn descriptor(&self) -> EvaluationFormatDescriptor {
        EvaluationFormatDescriptor {
            extension_id: EXTENSION_ID.to_owned(),
            adapter_id: ADAPTER_ID.to_owned(),
            revision: REVISION,
        }
    }

    fn render_request(
        &self,
        request: &EvaluationRequest,
        target: &EvaluationRoutingTarget,
    ) -> Result<Value> {
        request.validate()?;
        if target.provider_model_id != PROVIDER_MODEL_ID {
            return Err(BitrouterError::bad_request(
                "Laya provider model does not name the pinned checkpoint",
            ));
        }
        if request.questions.len() > MAX_QUESTIONS {
            return Err(BitrouterError::bad_request(
                "Laya local process accepts at most 16 questions",
            ));
        }
        for question in request.questions.values() {
            match question {
                EvaluationQuestion::Choice { criteria, .. }
                    if criteria.len() > MAX_CHOICE_OPTIONS =>
                {
                    return Err(BitrouterError::bad_request(
                        "Laya choice exceeds its local option limit",
                    ));
                }
                EvaluationQuestion::Score { criteria, .. } if criteria.len() > MAX_SCORE_LEVELS => {
                    return Err(BitrouterError::bad_request(
                        "Laya score exceeds its local level limit",
                    ));
                }
                _ => {}
            }
        }
        Ok(json!({
            "model": target.provider_model_id,
            "state": request.state,
            "questions": request.questions,
        }))
    }

    fn parse_response(
        &self,
        mut body: Value,
        request: &EvaluationRequest,
    ) -> Result<EvaluationResult> {
        validate_distribution_precision(&body, request)?;
        let answers = body
            .get_mut("answers")
            .and_then(Value::as_object_mut)
            .ok_or_else(invalid_answer)?;
        for answer in answers.values_mut() {
            let object = answer.as_object_mut().ok_or_else(invalid_answer)?;
            // Laya's auxiliary action head is not part of the public answer.
            object.remove("action");
            // Canonical Noul carries its probability, not Laya's derived
            // max(p, 1-p) confidence. Other answer confidences are preserved.
            if object.get("type").and_then(Value::as_str) == Some("noul") {
                object.remove("confidence");
            }
        }
        let parsed: LocalResponse = serde_json::from_value(body).map_err(|_| invalid_answer())?;
        if parsed.model != PROVIDER_MODEL_ID || parsed.usage.cost.is_some() {
            return Err(invalid_answer());
        }
        if parsed.answers.keys().ne(request.questions.keys()) {
            return Err(invalid_answer());
        }
        for (id, question) in &request.questions {
            let Some(answer) = parsed.answers.get(id) else {
                return Err(invalid_answer());
            };
            if let (
                EvaluationQuestion::Score { criteria, .. },
                EvaluationAnswer::Score { legend, .. },
            ) = (question, answer)
                && (legend.len() != criteria.len()
                    || criteria.iter().enumerate().any(|(index, criterion)| {
                        legend.get(&index.to_string()) != Some(criterion)
                    }))
            {
                return Err(invalid_answer());
            }
        }
        let result = EvaluationResult {
            id: "host-overwrites-id".to_owned(),
            model: parsed.model,
            provider: "host-overwrites-provider".to_owned(),
            answers: parsed.answers,
            usage: parsed.usage,
        };
        result.validate_against(request)?;
        Ok(result)
    }
}

fn invalid_answer() -> BitrouterError {
    BitrouterError::UpstreamInvalidResponse {
        message: "Laya returned a malformed evaluation answer".to_owned(),
    }
}

/// The pinned Laya runtime rounds each choice/score probability to four
/// decimal places. Summing the displayed values may therefore drift by at
/// most half a 0.0001 unit per option; no renormalization is performed.
fn validate_distribution_precision(body: &Value, request: &EvaluationRequest) -> Result<()> {
    let answers = body
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(invalid_answer)?;
    for (id, question) in &request.questions {
        if !matches!(
            question,
            EvaluationQuestion::Choice { .. } | EvaluationQuestion::Score { .. }
        ) {
            continue;
        }
        let probabilities = answers
            .get(id)
            .and_then(|answer| answer.get("probabilities"))
            .and_then(Value::as_object)
            .ok_or_else(invalid_answer)?;
        if probabilities.is_empty() {
            return Err(invalid_answer());
        }
        let mut sum = 0.0;
        for probability in probabilities.values() {
            let value = probability.as_f64().ok_or_else(invalid_answer)?;
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(invalid_answer());
            }
            sum += value;
        }
        let rounding_bound = probabilities.len() as f64 * 0.00005 + 1e-12;
        if (sum - 1.0).abs() > rounding_bound {
            return Err(invalid_answer());
        }
    }
    Ok(())
}
