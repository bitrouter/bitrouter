//! Typed evidence, questions and answers for native decision model calls.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::types::Usage;

/// Evidence and questions evaluated by one selected model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRequest {
    /// Caller-facing selector; invocation projects the selected native model.
    pub model: String,
    /// Shared text or inline image evidence.
    pub input: DecisionInput,
    /// Independent questions, in answer order.
    pub questions: Vec<DecisionQuestion>,
    /// Caller-supplied opaque identity; it grants no authentication authority.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_nullable"
    )]
    pub safety_identifier: Option<Option<String>>,
}

/// Model-visible text categories for operation-aware admission and estimation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionTextKind {
    /// Shared evidence text.
    Evidence,
    /// Caller-supplied question name.
    QuestionName,
    /// Question criteria.
    Instructions,
    /// A string-valued choice, distinct from a boolean spelling.
    StringChoice,
    /// A boolean-valued choice.
    BooleanChoice,
    /// Choice criteria.
    ChoiceDescription,
    /// Rubric label.
    LevelLabel,
    /// Rubric criteria.
    LevelDescription,
}

impl DecisionRequest {
    /// Visit covered text in evidence/question order, excluding identity/images.
    pub fn visit_text(&self, mut visitor: impl FnMut(DecisionTextKind, Option<usize>, &str)) {
        match &self.input {
            DecisionInput::Text(text) => visitor(DecisionTextKind::Evidence, None, text),
            DecisionInput::Messages(messages) => {
                for message in messages {
                    match &message.content {
                        DecisionContent::Text(text) => {
                            visitor(DecisionTextKind::Evidence, None, text)
                        }
                        DecisionContent::Parts(parts) => {
                            for part in parts {
                                if let DecisionInputPart::InputText { text } = part {
                                    visitor(DecisionTextKind::Evidence, None, text);
                                }
                            }
                        }
                    }
                }
            }
        }
        for (index, question) in self.questions.iter().enumerate() {
            let index = Some(index);
            if let Some(name) = question.name() {
                visitor(DecisionTextKind::QuestionName, index, name);
            }
            visitor(
                DecisionTextKind::Instructions,
                index,
                question.instructions(),
            );
            match question {
                DecisionQuestion::Predicate { .. } => {}
                DecisionQuestion::Choice { choices, .. } => {
                    for choice in choices {
                        match &choice.value {
                            DecisionChoiceValue::String(value) => {
                                visitor(DecisionTextKind::StringChoice, index, value)
                            }
                            DecisionChoiceValue::Boolean(value) => visitor(
                                DecisionTextKind::BooleanChoice,
                                index,
                                if *value { "true" } else { "false" },
                            ),
                        }
                        if let Some(description) = &choice.description {
                            visitor(DecisionTextKind::ChoiceDescription, index, description);
                        }
                    }
                }
                DecisionQuestion::Score { levels, .. } => {
                    for level in levels {
                        visitor(DecisionTextKind::LevelLabel, index, &level.label);
                        if let Some(description) = &level.description {
                            visitor(DecisionTextKind::LevelDescription, index, description);
                        }
                    }
                }
            }
        }
    }

    /// Inline image count, separate from covered text.
    pub fn image_count(&self) -> u64 {
        let DecisionInput::Messages(messages) = &self.input else {
            return 0;
        };
        messages
            .iter()
            .map(|message| match &message.content {
                DecisionContent::Text(_) => 0,
                DecisionContent::Parts(parts) => parts
                    .iter()
                    .filter(|part| matches!(part, DecisionInputPart::InputImage { .. }))
                    .count() as u64,
            })
            .fold(0, u64::saturating_add)
    }

    /// Rough text count for host estimates; it is never provider usage proof.
    pub fn text_chars(&self) -> u64 {
        let mut chars = 0_u64;
        self.visit_text(|_, _, text| chars = chars.saturating_add(text.chars().count() as u64));
        chars
    }
}

/// Shared decision evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DecisionInput {
    /// Plain text evidence.
    Text(String),
    /// Ordered user messages containing text or inline images.
    Messages(Vec<DecisionMessage>),
}

/// The only role admitted by native Decisions input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionRole {
    /// User-supplied evidence.
    User,
}

/// Optional native input message discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMessageType {
    /// An evidence message.
    Message,
}

/// A native user evidence message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionMessage {
    /// Native role, which can only be user.
    pub role: DecisionRole,
    /// Optional native discriminator.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub message_type: Option<DecisionMessageType>,
    /// Ordered evidence content.
    pub content: DecisionContent,
}

/// Text or ordered native content parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DecisionContent {
    /// Plain text.
    Text(String),
    /// Text and inline images.
    Parts(Vec<DecisionInputPart>),
}

/// Native Decisions image detail choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionImageDetail {
    /// Low-detail processing.
    Low,
    /// High-detail processing.
    High,
    /// Provider-selected detail.
    Auto,
    /// Original image detail.
    Original,
}

/// One text or inline-image evidence part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecisionInputPart {
    /// Text evidence.
    InputText {
        /// Evidence text.
        text: String,
    },
    /// An inline base64 data URL; external URLs and file IDs are not admitted.
    InputImage {
        /// Native data URL.
        image_url: String,
        /// Optional native image detail.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_nullable"
        )]
        detail: Option<Option<DecisionImageDetail>>,
    },
}

/// A typed category value; boolean true differs from string "true".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DecisionChoiceValue {
    /// String category.
    String(String),
    /// Boolean category.
    Boolean(bool),
}

/// One supplied choice and its optional meaning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionChoice {
    /// Typed category value.
    pub value: DecisionChoiceValue,
    /// Optional criteria for this choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One ordered rubric level, whose index starts at zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionLevel {
    /// Native level label.
    pub label: String,
    /// Optional criteria for this level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A predicate, category choice or ordered score question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecisionQuestion {
    /// Estimate whether a statement holds.
    Predicate {
        /// Question instructions.
        instructions: String,
        /// Optional answer identity; position remains authoritative.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Choose one supplied typed value.
    Choice {
        /// Question instructions.
        instructions: String,
        /// Supplied categories, in native order.
        choices: Vec<DecisionChoice>,
        /// Optional answer identity.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Evaluate an ordered rubric.
    Score {
        /// Question instructions.
        instructions: String,
        /// Supplied rubric levels, lowest first.
        levels: Vec<DecisionLevel>,
        /// Optional answer identity.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl DecisionQuestion {
    /// Caller-supplied name, without replacing positional identity.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Predicate { name, .. } | Self::Choice { name, .. } | Self::Score { name, .. } => {
                name.as_deref()
            }
        }
    }

    /// Instructions defining this question's criteria.
    pub fn instructions(&self) -> &str {
        match self {
            Self::Predicate { instructions, .. }
            | Self::Choice { instructions, .. }
            | Self::Score { instructions, .. } => instructions,
        }
    }
}

/// A native per-choice probability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionChoiceProbability {
    /// Supplied typed choice value.
    pub value: DecisionChoiceValue,
    /// Provider estimate in [0, 1].
    pub probability: f64,
    /// Same-wire additive fields, retained without interpretation.
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// A native per-level probability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionScoreProbability {
    /// Zero-based rubric level index.
    pub value: u32,
    /// Supplied level label.
    pub label: String,
    /// Provider estimate in [0, 1].
    pub probability: f64,
    /// Same-wire additive fields.
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// One ordered typed answer or refusal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionAnswer {
    /// Estimated predicate probability.
    Predicate {
        /// Echoed question name, or null when unnamed.
        name: Option<String>,
        /// Estimated probability in [0, 1].
        probability: f64,
        /// Same-wire additive fields.
        #[serde(default, flatten)]
        extensions: BTreeMap<String, serde_json::Value>,
    },
    /// Selected category and its distribution.
    Choice {
        /// Echoed question name.
        name: Option<String>,
        /// Selected supplied typed value.
        choice: DecisionChoiceValue,
        /// Native probability distribution.
        probabilities: Vec<DecisionChoiceProbability>,
        /// Provider confidence; it is not an application accuracy measurement.
        confidence: f64,
        /// Same-wire additive fields.
        #[serde(default, flatten)]
        extensions: BTreeMap<String, serde_json::Value>,
    },
    /// Probability-weighted rubric level index.
    Score {
        /// Echoed question name.
        name: Option<String>,
        /// Native weighted score, without rounding.
        score: f64,
        /// Native per-level distribution.
        probabilities: Vec<DecisionScoreProbability>,
        /// Provider confidence, preserved independently of the distribution.
        confidence: f64,
        /// Same-wire additive fields.
        #[serde(default, flatten)]
        extensions: BTreeMap<String, serde_json::Value>,
    },
    /// This question was declined; other answers may still succeed.
    Refusal {
        /// Echoed question name.
        name: Option<String>,
        /// Same-wire additive fields.
        #[serde(default, flatten)]
        extensions: BTreeMap<String, serde_json::Value>,
    },
}

impl DecisionAnswer {
    /// Echoed name, retaining positional answer correspondence.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Predicate { name, .. }
            | Self::Choice { name, .. }
            | Self::Score { name, .. }
            | Self::Refusal { name, .. } => name.as_deref(),
        }
    }
}

/// A complete decision result, without generative content or finish reasons.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionResult {
    /// Actual provider-reported model.
    pub model: String,
    /// Answers in request order.
    pub answers: Vec<DecisionAnswer>,
    /// Validated canonical usage, retaining the native usage object.
    pub usage: Usage,
    /// Same-wire additive envelope fields.
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Evidence retained when a complete provider response fails validation.
#[derive(Clone)]
pub struct DecisionResponseFailure {
    /// Content-free structural diagnostic.
    pub message: String,
    /// Independently validated usage, even when answers are malformed.
    pub usage: Option<Box<Usage>>,
}

impl fmt::Display for DecisionResponseFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl fmt::Debug for DecisionResponseFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecisionResponseFailure")
            .field("message", &self.message)
            .field("usage_available", &self.usage.is_some())
            .finish()
    }
}

fn present_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}
