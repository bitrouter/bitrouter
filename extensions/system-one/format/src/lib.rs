//! Native System One JSON facet for explicitly linked custom hosts.
//!
//! This crate owns no HTTP client, endpoint, credential, retry policy, or
//! settlement store. The stock `bro` binary does not link or register it.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use bitrouter_sdk::error::{BitrouterError, Result};
use bitrouter_sdk::evaluation::{
    EvaluationAnswer, EvaluationQuestion, EvaluationRequest, EvaluationResult,
    EvaluationRoutingTarget, EvaluationUsage,
};
use bitrouter_sdk::extension::ExtensionApi;
use bitrouter_sdk::extension::evaluation_format::{
    EvaluationFormatAdapter, EvaluationFormatDescriptor,
};
use serde::Deserialize;
use serde_json::{Number, Value, json};

/// The exact native format registered by this crate.
pub const EXTENSION_ID: &str = "system-one";
/// The facet scoped to [`EXTENSION_ID`].
pub const ADAPTER_ID: &str = "json";
/// Contract revision initially verified against the TypeSafe provider route.
pub const REVISION: u32 = 1;

/// Register the System One facet into a trusted custom host.
pub fn register(api: &mut ExtensionApi) -> Result<()> {
    api.register_evaluation_format(Arc::new(SystemOneFormat))
}

/// System One's non-streaming JSON dialect, initially verified with TypeSafe.
pub struct SystemOneFormat;

#[derive(Deserialize)]
struct SystemOneResponse {
    model: String,
    answers: BTreeMap<String, EvaluationAnswer>,
    usage: EvaluationUsage,
}

impl EvaluationFormatAdapter for SystemOneFormat {
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
        if target.provider_model_id.is_empty() {
            return Err(BitrouterError::bad_request(
                "provider model id must be non-empty",
            ));
        }
        Ok(json!({
            "model": target.provider_model_id,
            "state": request.state,
            "questions": request.questions,
        }))
    }

    fn parse_response(&self, body: Value, request: &EvaluationRequest) -> Result<EvaluationResult> {
        validate_distribution_precision(&body, request)?;
        let parsed: SystemOneResponse =
            serde_json::from_value(body).map_err(|_| invalid_answer())?;
        if parsed.model.is_empty() {
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
        message: "upstream returned a malformed System One evaluation answer".to_owned(),
    }
}

/// Accept the maximum error implied by rounding each visible probability to
/// its serialized decimal precision, capped at five percentage points. This
/// is deliberately narrower than a universal tolerance for coarse/invalid
/// distributions. The live-provider smoke gate must confirm the precision
/// TypeSafe actually emits before Phase 2 is declared complete.
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
        let mut rounding_bound = 0.0;
        for probability in probabilities.values() {
            let number = probability.as_number().ok_or_else(invalid_answer)?;
            let value = number.as_f64().ok_or_else(invalid_answer)?;
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(invalid_answer());
            }
            sum += value;
            rounding_bound += decimal_half_unit(number);
        }
        if (sum - 1.0).abs() > rounding_bound.min(0.05) + 1e-12 {
            return Err(invalid_answer());
        }
    }
    Ok(())
}

fn decimal_half_unit(number: &Number) -> f64 {
    let value = number.as_f64().unwrap_or(0.0);
    if value == 0.0 || value == 1.0 {
        return 0.0;
    }
    let text = number.to_string();
    let (mantissa, exponent) = text
        .split_once(['e', 'E'])
        .map_or((text.as_str(), 0), |(m, e)| {
            (m, e.parse::<i32>().unwrap_or(0))
        });
    let decimals = mantissa
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    0.5 * 10_f64.powi(exponent.saturating_sub(i32::try_from(decimals).unwrap_or(i32::MAX)))
}
