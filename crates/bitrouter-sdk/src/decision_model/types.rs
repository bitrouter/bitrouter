//! The TypeSafe question/answer contract and strict semantic validation.
//!
//! Wire reference: <https://api.typesafe.ai/openapi.json>. Question identifiers
//! are bookkeeping, not model input: instructions must identify their subject.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Shared state and independent semantic questions evaluated in one call.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DecisionRequest {
    /// Exact decision model to use.
    pub model: String,
    /// A string, object or array, containing the evidence to judge.
    pub state: Value,
    /// Stable question identifiers, each with its own explicit rubric.
    pub questions: BTreeMap<String, Question>,
}

/// A typed semantic question. Questions cannot depend on answers in this batch.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// Choose one named alternative.
    Choice {
        /// Explicit subject and task. IDs themselves are not shown to the model.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Value>,
        /// Allowed labels and their descriptions (null descriptions are allowed).
        criteria: BTreeMap<String, Value>,
    },
    /// Judge an ordered rubric; the returned score can be fractional.
    Score {
        /// Explicit subject and task.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Value>,
        /// Ordered rubric descriptions, indexed from zero in the response.
        criteria: Vec<Value>,
    },
    /// Estimate the probability of the true criterion.
    Noul {
        /// Explicit proposition to judge.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Value>,
        /// Descriptions of the true and false alternatives.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
}

/// Optional descriptions for a Noul proposition.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NoulCriteria {
    /// Meaning of a true answer.
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub positive: Option<Value>,
    /// Meaning of a false answer.
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub negative: Option<Value>,
}

/// Provider-reported token usage. Absence of usage is never treated as zero.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecisionUsage {
    /// Input tokens reported by the provider.
    pub input_tokens: u64,
    /// Output tokens reported by the provider.
    pub output_tokens: u64,
}

/// Answers and observed usage from one decision attempt.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DecisionResponse {
    /// Provider-reported decision model.
    pub model: String,
    /// Exactly one answer for each requested question.
    pub answers: BTreeMap<String, Answer>,
    /// Reported tokens, including all questions in the batch.
    pub usage: DecisionUsage,
}

/// A typed answer. Probabilities and labels are checked against the request.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    /// One selected alternative and the full categorical distribution.
    Choice {
        /// Selected criterion label.
        choice: String,
        /// Provider confidence in the choice, in the unit interval.
        confidence: f64,
        /// Distribution over exactly the requested labels.
        probabilities: BTreeMap<String, f64>,
    },
    /// Expected rubric score and the full ordinal distribution.
    Score {
        /// Weighted mean of zero-based rubric indices, not an integer label.
        score: f64,
        /// Provider confidence, in the unit interval.
        confidence: f64,
        /// Provider echo of the indexed rubric.
        legend: BTreeMap<String, Value>,
        /// Distribution over exactly the requested rubric indices.
        probabilities: BTreeMap<String, f64>,
    },
    /// Probability of the proposition being true; no separate confidence field.
    Noul {
        /// Probability in the unit interval.
        noul: f64,
    },
}

/// Machine-readable failure category, suitable for a durable attempt receipt.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionFailure {
    /// The request cannot be sent as a valid typed decision.
    InvalidRequest,
    /// Network failure after dispatch may have incurred provider cost.
    Transport,
    /// Non-success HTTP status. No provider response body is exposed.
    Http,
    /// An answer failed decoding or did not satisfy its requested rubric.
    InvalidResponse,
    /// The response exceeded the configured byte limit.
    ResponseTooLarge,
    /// The configured deadline expired.
    Timeout,
    /// The caller cancelled this attempt.
    Cancelled,
    /// The previous owner stopped before recording the attempt's result.
    Interrupted,
}

/// Sanitized failure with billing evidence retained even when answers are invalid.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, thiserror::Error)]
#[error("decision {kind:?}: {message}")]
pub struct DecisionError {
    /// Failure category.
    pub kind: DecisionFailure,
    /// Safe diagnostic, excluding credentials and provider response bodies.
    pub message: String,
    /// Known usage if a provider response could be read.
    pub usage: Option<DecisionUsage>,
    /// True if dispatch began; missing usage then means unknown cost.
    pub may_have_run: bool,
}

impl DecisionError {
    /// A failure before any provider dispatch.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            kind: DecisionFailure::InvalidRequest,
            message: message.into(),
            usage: None,
            may_have_run: false,
        }
    }
}

impl DecisionRequest {
    /// Check the request without contacting a provider.
    pub fn validate(&self) -> Result<(), DecisionError> {
        if self.model.trim().is_empty() || !is_material(&self.state) || self.questions.is_empty() {
            return Err(DecisionError::invalid_request(
                "model, structured state and at least one question are required",
            ));
        }
        for (id, question) in &self.questions {
            if id.trim().is_empty() {
                return Err(DecisionError::invalid_request("question ID is empty"));
            }
            let (instructions, valid_criteria) = match question {
                Question::Choice {
                    instructions,
                    criteria,
                } => (
                    instructions,
                    !criteria.is_empty()
                        && criteria.iter().all(|(label, value)| {
                            !label.trim().is_empty() && (value.is_null() || is_material(value))
                        }),
                ),
                Question::Score {
                    instructions,
                    criteria,
                } => (
                    instructions,
                    !criteria.is_empty()
                        && criteria.len() <= 10
                        && criteria.iter().all(is_material),
                ),
                Question::Noul {
                    instructions,
                    criteria,
                } => (
                    instructions,
                    criteria.as_ref().is_none_or(|criteria| {
                        [&criteria.positive, &criteria.negative]
                            .into_iter()
                            .all(|value| value.as_ref().is_none_or(is_material))
                    }),
                ),
            };
            if !valid_criteria || !instructions.as_ref().is_none_or(is_material) {
                return Err(DecisionError::invalid_request("invalid question rubric"));
            }
        }
        Ok(())
    }
}

impl DecisionResponse {
    /// Reject missing/extra answers, type mismatches and invalid distributions.
    pub fn validate(&self, request: &DecisionRequest) -> Result<(), DecisionError> {
        let invalid = || DecisionError {
            kind: DecisionFailure::InvalidResponse,
            message: "answers do not match the requested rubric".into(),
            usage: Some(self.usage),
            may_have_run: true,
        };
        // The provider may resolve a requested alias to a concrete model name.
        if self.model.trim().is_empty() || !self.answers.keys().eq(request.questions.keys()) {
            return Err(invalid());
        }
        for (id, question) in &request.questions {
            let Some(answer) = self.answers.get(id) else {
                return Err(invalid());
            };
            let valid = match (question, answer) {
                (Question::Noul { .. }, Answer::Noul { noul }) => unit(*noul),
                (
                    Question::Choice { criteria, .. },
                    Answer::Choice {
                        choice,
                        confidence,
                        probabilities,
                    },
                ) => {
                    unit(*confidence)
                        && criteria.contains_key(choice)
                        && criteria.keys().eq(probabilities.keys())
                        && distribution(probabilities)
                        && probabilities.get(choice).is_some_and(|selected| {
                            probabilities.values().all(|value| value <= selected)
                        })
                }
                (
                    Question::Score { criteria, .. },
                    Answer::Score {
                        score,
                        confidence,
                        legend,
                        probabilities,
                    },
                ) => {
                    let expected: BTreeMap<_, _> = criteria
                        .iter()
                        .enumerate()
                        .map(|(index, value)| (index.to_string(), value.clone()))
                        .collect();
                    let mean: f64 = criteria
                        .iter()
                        .enumerate()
                        .map(|(index, _)| {
                            index as f64
                                * probabilities
                                    .get(&index.to_string())
                                    .copied()
                                    .unwrap_or(0.0)
                        })
                        .sum();
                    unit(*confidence)
                        && score.is_finite()
                        && *score >= 0.0
                        && *score <= criteria.len().saturating_sub(1) as f64
                        && expected == *legend
                        && expected.keys().eq(probabilities.keys())
                        && distribution(probabilities)
                        && (*score - mean).abs() <= 0.01
                }
                _ => false,
            };
            if !valid {
                return Err(invalid());
            }
        }
        Ok(())
    }
}

fn is_material(value: &Value) -> bool {
    value.is_string() || value.is_array() || value.is_object()
}

fn unit(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn distribution(values: &BTreeMap<String, f64>) -> bool {
    !values.is_empty()
        && values.values().all(|value| unit(*value))
        && (values.values().sum::<f64>() - 1.0).abs() <= 0.01
}
