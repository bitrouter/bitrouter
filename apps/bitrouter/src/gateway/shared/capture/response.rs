//! A bounded private encoding of the existing classifier's typed observation.
//!
//! Neither raw response content nor an invocation token enters durable inputs.

#[cfg(test)]
mod tests;

use super::*;
use crate::gateway::observability::prediction::{ObservedActionClass, PredictionObservation};

// Includes the enclosing JSON field name and punctuation. All legal encodings
// are ASCII and fixed by the classifier contract, independently of input text.
pub(super) const MAX_RESPONSE_BYTES: usize = 256;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResponseObservation {
    observed_action: String,
    confidence_ppm: u32,
    reason_code: String,
}

impl From<PredictionObservation> for ResponseObservation {
    fn from(value: PredictionObservation) -> Self {
        Self {
            observed_action: value.observed_action.as_str().into(),
            confidence_ppm: value.confidence_ppm(),
            reason_code: value.reason_code().into(),
        }
    }
}

impl ResponseObservation {
    pub(super) fn typed(&self) -> Result<PredictionObservation> {
        // Reject unrecognized/unbounded values; do not truncate evidence or
        // accept arbitrary reason strings from a persisted private envelope.
        ensure!(
            self.observed_action.len() <= 32 && self.reason_code.len() <= 32,
            "response observation label exceeds bound"
        );
        let action =
            ObservedActionClass::parse(&self.observed_action).context("invalid response action")?;
        let value = PredictionObservation::new(action);
        ensure!(
            self.confidence_ppm == value.confidence_ppm()
                && self.reason_code == value.reason_code(),
            "response observation differs from classifier contract"
        );
        Ok(value)
    }
}

pub(super) fn is_response_attribute(key: &str) -> bool {
    matches!(
        key,
        "observed_action"
            | "observation_confidence_ppm"
            | "observation_reason_code"
            | "action_match"
    )
}

impl Capture {
    pub(in crate::gateway::shared) fn observe_response(
        &self,
        execution_id: &str,
        owner: &str,
        observation: PredictionObservation,
    ) -> Result<()> {
        let observed = ResponseObservation::from(observation);
        observed.typed()?;
        self.update(|input| {
            input.check_identity(execution_id, owner, ModelOperation::Generation)?;
            // Seal and update share the existing capture lock. Late observations
            // cannot modify terminal inputs, even after an uncertain commit ACK.
            input.require_ready()?;
            input.response_observation = Some(match &input.response_observation {
                Some(prior) => prior.typed()?.merge(observation).into(),
                None => observed,
            });
            Ok(())
        })
    }
}

impl Inputs {
    pub(super) fn prediction_attributes(&self) -> BTreeMap<String, String> {
        let mut attributes = match &self.policy {
            Policy::Evaluated {
                treatment: Some(treatment),
                ..
            } => treatment.attributes.clone(),
            _ => BTreeMap::new(),
        };
        if let Some(observation) = &self.response_observation {
            attributes.insert(
                "observed_action".into(),
                observation.observed_action.clone(),
            );
            attributes.insert(
                "observation_confidence_ppm".into(),
                observation.confidence_ppm.to_string(),
            );
            attributes.insert(
                "observation_reason_code".into(),
                observation.reason_code.clone(),
            );
            if let (Some(predicted), Some(observed)) = (
                attributes
                    .get("predicted_action")
                    .and_then(|value| ObservedActionClass::parse(value))
                    .filter(|action| action.is_known()),
                ObservedActionClass::parse(&observation.observed_action)
                    .filter(|action| action.is_known()),
            ) {
                attributes.insert("action_match".into(), (predicted == observed).to_string());
            }
        }
        attributes
    }
}
