//! Bounded translation of the planner's Choice rubric to native Decisions.
use crate::gateway::model::{
    decision::types::{
        Answer, DecisionError, DecisionFailure, DecisionRequest, DecisionResponse, DecisionUsage,
        Question,
    },
    protocol::decisions::DecisionsCodec,
};
use bitrouter_ai::decisions::{
    DecisionAnswer, DecisionChoice, DecisionChoiceValue, DecisionInput, DecisionQuestion,
    DecisionRequest as NativeRequest, DecisionResult,
};
use serde::Serialize;
use std::{collections::BTreeMap, io::Write};

#[cfg(test)]
mod tests;

pub(crate) struct Prepared {
    pub request: NativeRequest,
    pub digest: String,
}

pub(crate) fn prepare(
    request: &DecisionRequest,
    child: &str,
    limit: usize,
) -> Result<Prepared, DecisionError> {
    request.validate()?;
    encoded(request, limit)
        .map_err(|_| DecisionError::invalid_request("owned Choice request exceeds its bound"))?;
    if request.model != child || request.questions.len() > 35 {
        return Err(DecisionError::invalid_request(
            "unsupported owned Choice request",
        ));
    }
    #[derive(Serialize)]
    struct Evidence<'a> {
        state: &'a serde_json::Value,
    }
    let evidence = encoded(
        &Evidence {
            state: &request.state,
        },
        limit,
    )
    .map_err(|_| DecisionError::invalid_request("owned Choice evidence exceeds its bound"))?;
    let input = String::from_utf8(evidence)
        .map_err(|_| DecisionError::invalid_request("invalid owned Choice evidence"))?;
    let mut questions = Vec::with_capacity(request.questions.len());
    for (name, question) in &request.questions {
        let Question::Choice {
            instructions: Some(serde_json::Value::String(instructions)),
            criteria,
        } = question
        else {
            return Err(DecisionError::invalid_request(
                "owned Choice requires text instructions and a Choice rubric",
            ));
        };
        if criteria.len() > 16 {
            return Err(DecisionError::invalid_request(
                "owned Choice rubric exceeds its bound",
            ));
        }
        let mut choices = Vec::with_capacity(criteria.len());
        for (value, description) in criteria {
            let description = match description {
                serde_json::Value::Null => None,
                serde_json::Value::String(text) => Some(text.clone()),
                _ => {
                    return Err(DecisionError::invalid_request(
                        "owned Choice descriptions must be text or null",
                    ));
                }
            };
            choices.push(DecisionChoice {
                value: DecisionChoiceValue::String(value.clone()),
                description,
            });
        }
        questions.push(DecisionQuestion::Choice {
            name: Some(name.clone()),
            instructions: instructions.clone(),
            choices,
        });
    }
    let request = NativeRequest {
        model: child.into(),
        input: DecisionInput::Text(input),
        questions,
        safety_identifier: None,
    };
    // The escaped translated request, not just the original state, owns the bound.
    // Native Decisions is a separate wire; it never becomes a Generation prompt.
    let bytes = encoded(&request, limit)
        .map_err(|_| DecisionError::invalid_request("translated owned Choice exceeds its bound"))?;
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| DecisionError::invalid_request("invalid owned Choice JSON"))?;
    let request = DecisionsCodec::parse_request(value)
        .map_err(|_| DecisionError::invalid_request("unsupported native Decisions request"))?;
    Ok(Prepared {
        request,
        digest: crate::storage::checkpoint::sha256(&bytes),
    })
}

pub(super) fn response(
    request: &DecisionRequest,
    native: &NativeRequest,
    result: &DecisionResult,
    limit: usize,
) -> Result<DecisionResponse, DecisionError> {
    // Recheck original provider usage; canonical zero defaults cannot prove it.
    let usage = result
        .usage
        .raw
        .as_deref()
        .and_then(|raw| {
            Some(DecisionUsage {
                input_tokens: raw.get("input_tokens")?.as_u64()?,
                output_tokens: raw.get("output_tokens")?.as_u64()?,
            })
        })
        .filter(|usage| {
            usage.input_tokens == result.usage.prompt_tokens
                && usage.output_tokens == result.usage.completion_tokens
        });
    let invalid = || DecisionError {
        kind: DecisionFailure::InvalidResponse,
        message: "native Choice answer or usage is invalid".into(),
        usage,
        may_have_run: true,
    };
    let usage = usage.ok_or_else(invalid)?;
    DecisionsCodec::render_response(result, native).map_err(|_| invalid())?;
    if result.answers.len() != request.questions.len() {
        return Err(invalid());
    }
    let mut answers = BTreeMap::new();
    for ((id, _), answer) in request.questions.iter().zip(&result.answers) {
        let DecisionAnswer::Choice {
            name,
            choice: DecisionChoiceValue::String(choice),
            probabilities,
            confidence,
            ..
        } = answer
        else {
            return Err(invalid());
        };
        if name.as_deref() != Some(id.as_str()) {
            return Err(invalid());
        }
        let mut distribution = BTreeMap::new();
        for probability in probabilities {
            let DecisionChoiceValue::String(label) = &probability.value else {
                return Err(invalid());
            };
            if distribution
                .insert(label.clone(), probability.probability)
                .is_some()
            {
                return Err(invalid());
            }
        }
        answers.insert(
            id.clone(),
            Answer::Choice {
                choice: choice.clone(),
                confidence: *confidence,
                probabilities: distribution,
            },
        );
    }
    let response = DecisionResponse {
        model: result.model.clone(),
        answers,
        usage,
    };
    response.validate(request)?;
    // The frozen receipt allowance covers the serialized Result envelope too.
    // This is the same representation checked by checkpoint validation.
    encoded(&Ok::<_, DecisionError>(&response), limit).map_err(|_| DecisionError {
        kind: DecisionFailure::ResponseTooLarge,
        message: "owned Choice outcome exceeds its reservation".into(),
        usage: Some(usage),
        may_have_run: true,
    })?;
    Ok(response)
}

pub(crate) fn encoded(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, ()> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("owned Choice byte bound"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Bounded {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut output, value).map_err(|_| ())?;
    Ok(output.bytes)
}
