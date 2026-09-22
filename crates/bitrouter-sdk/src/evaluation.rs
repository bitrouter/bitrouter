//! Canonical typed-evaluation data contract.
//!
//! This module defines the public request and result shapes without routing,
//! provider transport, or a Jev-specific adapter. Unknown top-level request
//! fields are ignored; recognized fields and nested question shapes are
//! validated before any future evaluation pipeline can dispatch them.

use std::collections::BTreeMap;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::error::{BitrouterError, Result};

/// Question kinds advertised by an evaluation-capable model route.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationQuestionType {
    /// A probability for a binary yes/no question.
    Noul,
    /// A probability distribution over named choices.
    Choice,
    /// A probability distribution over ordered levels.
    Score,
}

/// A JSON string, object, or array. Descendants may contain JSON scalars.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct StructuredValue(Value);

impl StructuredValue {
    /// Borrow the original JSON without stringifying its object or array form.
    pub fn as_value(&self) -> &Value {
        &self.0
    }

    /// Return the original JSON value.
    pub fn into_value(self) -> Value {
        self.0
    }
}

impl TryFrom<Value> for StructuredValue {
    type Error = BitrouterError;

    fn try_from(value: Value) -> Result<Self> {
        if matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_)) {
            Ok(Self(value))
        } else {
            Err(BitrouterError::bad_request(
                "structured value must be a string, object, or array",
            ))
        }
    }
}

impl<'de> Deserialize<'de> for StructuredValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

/// Explanations for both outcomes of a Noul question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BooleanCriteria {
    /// Description of the affirmative outcome.
    #[serde(rename = "true")]
    pub yes: StructuredValue,
    /// Description of the negative outcome.
    #[serde(rename = "false")]
    pub no: StructuredValue,
}

fn deserialize_unique_map<'de, D, T>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct UniqueMapVisitor<T>(std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>> Visitor<'de> for UniqueMapVisitor<T> {
        type Value = BTreeMap<String, T>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("an object with unique keys")
        }

        fn visit_map<M: MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut entries = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, T>()? {
                if entries.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate object key"));
                }
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(UniqueMapVisitor(std::marker::PhantomData))
}

/// One typed question about the shared state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvaluationQuestion {
    /// A yes/no question returned as the probability of yes.
    Noul {
        /// Question text or structured instructions.
        instructions: StructuredValue,
        /// Optional definitions of both outcomes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<BooleanCriteria>,
    },
    /// A choice from caller-named alternatives.
    Choice {
        /// Question text or structured instructions.
        instructions: StructuredValue,
        /// Option name to optional structured explanation.
        #[serde(deserialize_with = "deserialize_unique_map")]
        criteria: BTreeMap<String, Option<StructuredValue>>,
    },
    /// A score over an ordered rubric.
    Score {
        /// Question text or structured instructions.
        instructions: StructuredValue,
        /// Ordered level descriptions.
        criteria: Vec<StructuredValue>,
    },
}

impl EvaluationQuestion {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Noul { .. } => Ok(()),
            Self::Choice { criteria, .. } => {
                if criteria.len() < 2 {
                    return Err(BitrouterError::bad_request(
                        "choice criteria must contain at least 2 options",
                    ));
                }
                if criteria.keys().any(|key| key.is_empty()) {
                    return Err(BitrouterError::bad_request(
                        "choice option names must be non-empty",
                    ));
                }
                Ok(())
            }
            Self::Score { criteria, .. } => {
                if criteria.len() < 2 {
                    return Err(BitrouterError::bad_request(
                        "score criteria must contain at least 2 levels",
                    ));
                }
                Ok(())
            }
        }
    }
}

/// A provider-neutral typed-evaluation request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EvaluationRequest {
    /// Canonical model id or provider-pinned selector.
    pub model: String,
    /// Text or structured context evaluated by every question.
    pub state: StructuredValue,
    /// Caller-selected question ids mapped to typed questions.
    #[serde(deserialize_with = "deserialize_unique_map")]
    pub questions: BTreeMap<String, EvaluationQuestion>,
}

impl EvaluationRequest {
    /// Validate the recognized canonical fields before routing.
    pub fn validate(&self) -> Result<()> {
        if self.model.is_empty() {
            return Err(BitrouterError::bad_request("model must be non-empty"));
        }
        if self.questions.is_empty() {
            return Err(BitrouterError::bad_request(
                "questions must contain at least one question",
            ));
        }
        for (id, question) in &self.questions {
            if id.is_empty() {
                return Err(BitrouterError::bad_request(
                    "question ids must be non-empty",
                ));
            }
            question.validate()?;
        }
        Ok(())
    }
}

/// One answer in the canonical evaluation result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvaluationAnswer {
    /// Probability of the affirmative answer.
    Noul {
        /// A value in `[0, 1]`.
        noul: f64,
    },
    /// A chosen option and the full probability distribution.
    Choice {
        /// Selected option name.
        choice: String,
        /// Probability for every declared option.
        probabilities: BTreeMap<String, f64>,
        /// Provider-supplied confidence, if present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
    },
    /// A weighted score and distribution over declared levels.
    Score {
        /// Provider-returned interpolated score.
        score: f64,
        /// Probability for every declared level, keyed by its index.
        probabilities: BTreeMap<String, f64>,
        /// Level index to original rubric description.
        legend: BTreeMap<String, StructuredValue>,
        /// Provider-supplied confidence, if present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
    },
}

/// Provider-returned token counts and optional settled cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvaluationUsage {
    /// Provider-returned input tokens.
    pub input_tokens: u64,
    /// Provider-returned output tokens, even if output is free.
    pub output_tokens: u64,
    /// Settled cost when supported by evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

/// A provider-neutral typed-evaluation result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvaluationResult {
    /// Stable BitRouter evaluation request id.
    pub id: String,
    /// Actual provider-reported version, or configured provider model id.
    pub model: String,
    /// Selected BitRouter provider id.
    pub provider: String,
    /// Answers keyed by the original question ids.
    pub answers: BTreeMap<String, EvaluationAnswer>,
    /// Provider token usage and optional settled cost.
    pub usage: EvaluationUsage,
}

impl EvaluationResult {
    /// Check provider-independent answer invariants against the submitted
    /// question set. Provider-specific probability precision is checked by
    /// the concrete format adapter, not by this canonical contract.
    pub fn validate_against(&self, request: &EvaluationRequest) -> Result<()> {
        request.validate()?;
        if self.id.is_empty() || self.model.is_empty() || self.provider.is_empty() {
            return Err(invalid_result("evaluation identity is incomplete"));
        }
        if self.answers.keys().ne(request.questions.keys()) {
            return Err(invalid_result("answer keys do not match question keys"));
        }
        if self
            .usage
            .cost
            .is_some_and(|cost| !cost.is_finite() || cost < 0.0)
        {
            return Err(invalid_result("evaluation cost is invalid"));
        }
        for (id, question) in &request.questions {
            let Some(answer) = self.answers.get(id) else {
                return Err(invalid_result("answer is missing"));
            };
            match (question, answer) {
                (EvaluationQuestion::Noul { .. }, EvaluationAnswer::Noul { noul }) => {
                    if !valid_probability(*noul) {
                        return Err(invalid_result("noul probability is invalid"));
                    }
                }
                (
                    EvaluationQuestion::Choice { criteria, .. },
                    EvaluationAnswer::Choice {
                        choice,
                        probabilities,
                        confidence,
                    },
                ) => {
                    if !criteria.contains_key(choice)
                        || criteria.keys().ne(probabilities.keys())
                        || probabilities
                            .values()
                            .any(|value| !valid_probability(*value))
                        || confidence.is_some_and(|value| !valid_probability(value))
                    {
                        return Err(invalid_result("choice answer is invalid"));
                    }
                }
                (
                    EvaluationQuestion::Score { criteria, .. },
                    EvaluationAnswer::Score {
                        score,
                        probabilities,
                        legend,
                        confidence,
                    },
                ) => {
                    if !score.is_finite()
                        || probabilities.len() != criteria.len()
                        || probabilities.keys().ne(legend.keys())
                        || probabilities
                            .values()
                            .any(|value| !valid_probability(*value))
                        || confidence.is_some_and(|value| !valid_probability(value))
                    {
                        return Err(invalid_result("score answer is invalid"));
                    }
                }
                _ => return Err(invalid_result("answer type does not match question type")),
            }
        }
        Ok(())
    }
}

fn valid_probability(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn invalid_result(message: &str) -> BitrouterError {
    BitrouterError::UpstreamInvalidResponse {
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{EvaluationAnswer, EvaluationQuestion, EvaluationRequest, EvaluationResult};

    #[test]
    fn mixed_questions_keep_structured_json_and_null_descriptions()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let input = json!({
            "model": "typesafe/jev-1.13",
            "state": {"nested": [1, true, null, {"threshold": 0.75}]},
            "questions": {
                "binary": {"type": "noul", "instructions": ["decide", {"tag": "urgent"}], "criteria": {"true": "yes", "false": {"why": "no"}}},
                "choice": {"type": "choice", "instructions": "choose", "criteria": {"a": null, "b": ["details"]}},
                "score": {"type": "score", "instructions": {"rubric": "quality"}, "criteria": ["low", {"level": 2}, ["high"]]}
            },
            "future_gateway_field": {"provider": "ignored"}
        });
        let request: EvaluationRequest = serde_json::from_value(input.clone())?;
        request.validate()?;
        assert_eq!(request.state.as_value(), &input["state"]);
        assert_eq!(request.questions.len(), 3);
        assert!(matches!(
            request.questions.get("binary"),
            Some(EvaluationQuestion::Noul { .. })
        ));
        assert!(matches!(
            request.questions.get("choice"),
            Some(EvaluationQuestion::Choice { .. })
        ));
        assert!(matches!(
            request.questions.get("score"),
            Some(EvaluationQuestion::Score { .. })
        ));
        let output = serde_json::to_value(&request)?;
        assert_eq!(output["state"], input["state"]);
        assert_eq!(
            output["questions"]["binary"]["instructions"],
            input["questions"]["binary"]["instructions"]
        );
        assert_eq!(output["questions"]["choice"]["criteria"]["a"], Value::Null);
        assert_eq!(
            output["questions"]["score"]["criteria"],
            input["questions"]["score"]["criteria"]
        );
        assert!(output.get("future_gateway_field").is_none());
        Ok(())
    }

    #[test]
    fn rejects_invalid_request_shapes_and_duplicate_keys()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        for state in [json!(null), json!(true), json!(4)] {
            let value = json!({"model": "m", "state": state, "questions": {"q": {"type": "noul", "instructions": "decide"}}});
            assert!(serde_json::from_value::<EvaluationRequest>(value).is_err());
        }
        for question in [
            json!({"type": "boolean", "instructions": "decide"}),
            json!({"type": "choice", "instructions": "choose", "criteria": {"one": null}}),
            json!({"type": "score", "instructions": "score", "criteria": ["only one"]}),
            json!({"type": "noul", "instructions": null}),
            json!({"type": "noul", "instructions": "decide", "unknown": true}),
        ] {
            let value = json!({"model": "m", "state": "context", "questions": {"q": question}});
            if let Ok(request) = serde_json::from_value::<EvaluationRequest>(value) {
                assert!(request.validate().is_err());
            }
        }
        let duplicate_questions = r#"{"model":"m","state":"s","questions":{"q":{"type":"noul","instructions":"a"},"q":{"type":"noul","instructions":"b"}}}"#;
        assert!(serde_json::from_str::<EvaluationRequest>(duplicate_questions).is_err());
        let duplicate_options = r#"{"model":"m","state":"s","questions":{"q":{"type":"choice","instructions":"a","criteria":{"x":null,"x":null,"y":null}}}}"#;
        assert!(serde_json::from_str::<EvaluationRequest>(duplicate_options).is_err());
        let empty_questions: EvaluationRequest =
            serde_json::from_value(json!({"model":"m","state":"s","questions":{}}))?;
        assert!(empty_questions.validate().is_err());
        let many_options = (0..256)
            .map(|index| (format!("option-{index}"), Value::Null))
            .collect::<serde_json::Map<_, _>>();
        let provider_independent: EvaluationRequest = serde_json::from_value(json!({
            "model": "m",
            "state": "s",
            "questions": {
                "choice": {"type": "choice", "instructions": "choose", "criteria": many_options},
                "score": {"type": "score", "instructions": "score", "criteria": (0..11).map(|index| format!("level-{index}")).collect::<Vec<_>>()}
            }
        }))?;
        provider_independent.validate()?;
        Ok(())
    }

    #[test]
    fn answer_variants_preserve_probabilities_legend_identity_and_usage()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let value = json!({
            "id": "eval_1", "model": "jev-1.13.0", "provider": "typesafe",
            "answers": {
                "n": {"type": "noul", "noul": 0.9},
                "c": {"type": "choice", "choice": "a", "probabilities": {"a": 0.7, "b": 0.3}, "confidence": 0.8},
                "s": {"type": "score", "score": 1.5, "probabilities": {"0": 0.5, "1": 0.5}, "legend": {"0": "low", "1": {"level": "high"}}}
            },
            "usage": {"input_tokens": 42, "output_tokens": 3, "cost": 0.000001764}
        });
        let result: EvaluationResult = serde_json::from_value(value.clone())?;
        assert!(matches!(
            result.answers.get("n"),
            Some(EvaluationAnswer::Noul { .. })
        ));
        assert!(matches!(
            result.answers.get("c"),
            Some(EvaluationAnswer::Choice { .. })
        ));
        assert!(matches!(
            result.answers.get("s"),
            Some(EvaluationAnswer::Score { .. })
        ));
        let request: EvaluationRequest = serde_json::from_value(json!({
            "model": "typesafe/jev-1.13",
            "state": "context",
            "questions": {
                "n": {"type": "noul", "instructions": "binary"},
                "c": {"type": "choice", "instructions": "choose", "criteria": {"a": null, "b": null}},
                "s": {"type": "score", "instructions": "score", "criteria": ["low", "high"]}
            }
        }))?;
        result.validate_against(&request)?;
        let mut invalid = result.clone();
        if let Some(EvaluationAnswer::Noul { noul }) = invalid.answers.get_mut("n") {
            *noul = 1.1;
        }
        assert!(invalid.validate_against(&request).is_err());
        let mut invalid = result.clone();
        invalid.answers.remove("n");
        assert!(invalid.validate_against(&request).is_err());
        let mut invalid = result.clone();
        invalid
            .answers
            .insert("extra".to_string(), EvaluationAnswer::Noul { noul: 0.5 });
        assert!(invalid.validate_against(&request).is_err());
        let mut invalid = result.clone();
        invalid.usage.cost = Some(-1.0);
        assert!(invalid.validate_against(&request).is_err());
        let mut extra = value.clone();
        extra["answers"]["n"]["unexpected"] = json!(true);
        assert!(serde_json::from_value::<EvaluationResult>(extra).is_err());
        assert_eq!(serde_json::to_value(result)?, value);
        Ok(())
    }
}
