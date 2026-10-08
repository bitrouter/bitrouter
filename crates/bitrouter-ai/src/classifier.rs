//! Typed evidence, questions and answers for native decision model calls.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::types::{ApiProtocol, Usage};

/// Evidence and questions evaluated by one selected model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierRequest {
    /// Caller-facing selector; invocation projects the selected native model.
    pub model: String,
    /// Original classifier wire, when parsed through a codec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_protocol: Option<ApiProtocol>,
    /// Shared text or inline image evidence.
    pub input: ClassifierInput,
    /// Independent questions, in answer order.
    pub questions: Vec<ClassifierQuestion>,
    /// Caller-supplied opaque identity; it grants no authentication authority.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_nullable"
    )]
    pub safety_identifier: Option<Option<String>>,
}

/// Model-visible text or structured JSON; scalar non-text roots are invalid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClassifierText {
    /// Plain text.
    Text(String),
    /// Explicit null used by nullable native instruction fields.
    Null,
    /// Object or array with typed nested fields.
    Structured(serde_json::Value),
}

impl From<String> for ClassifierText {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for ClassifierText {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl ClassifierText {
    /// Borrow plain text without implicitly serializing structured instructions.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Null | Self::Structured(_) => None,
        }
    }

    /// Whether the model-visible root is supported by System One.
    pub fn is_valid(&self) -> bool {
        match self {
            Self::Text(_) => true,
            Self::Null => false,
            Self::Structured(value) => value.is_object() || value.is_array(),
        }
    }

    /// Visit structured keys and values for request checks and estimates.
    pub fn visit_text(&self, visitor: &mut impl FnMut(&str)) {
        match self {
            Self::Text(text) => visitor(text),
            Self::Null => {}
            Self::Structured(value) => visit_json(value, visitor),
        }
    }
}

fn visit_json(value: &serde_json::Value, visitor: &mut impl FnMut(&str)) {
    match value {
        serde_json::Value::String(text) => visitor(text),
        serde_json::Value::Array(values) => {
            for value in values {
                visit_json(value, visitor);
            }
        }
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                visitor(key);
                visit_json(value, visitor);
            }
        }
        other => visitor(&other.to_string()),
    }
}

/// Optional descriptions of the affirmative and negative predicate outcomes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierPredicateCriteria {
    /// What a positive result means.
    #[serde(
        default,
        rename = "true",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_nullable"
    )]
    pub positive: Option<Option<ClassifierText>>,
    /// What a negative result means.
    #[serde(
        default,
        rename = "false",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_nullable"
    )]
    pub negative: Option<Option<ClassifierText>>,
}

/// Model-visible text categories for operation-aware admission and estimation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassifierTextKind {
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

impl ClassifierRequest {
    /// Visit covered text in evidence/question order, excluding identity/images.
    pub fn visit_text(&self, mut visitor: impl FnMut(ClassifierTextKind, Option<usize>, &str)) {
        match &self.input {
            ClassifierInput::Text(text) => visitor(ClassifierTextKind::Evidence, None, text),
            ClassifierInput::Structured(value) => visit_json(value, &mut |text| {
                visitor(ClassifierTextKind::Evidence, None, text)
            }),
            ClassifierInput::Messages(messages) => {
                for message in messages {
                    match &message.content {
                        ClassifierContent::Text(text) => {
                            visitor(ClassifierTextKind::Evidence, None, text)
                        }
                        ClassifierContent::Parts(parts) => {
                            for part in parts {
                                if let ClassifierInputPart::InputText { text } = part {
                                    visitor(ClassifierTextKind::Evidence, None, text);
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
                visitor(ClassifierTextKind::QuestionName, index, name);
            }
            if let Some(instructions) = question.instructions() {
                instructions
                    .visit_text(&mut |text| visitor(ClassifierTextKind::Instructions, index, text));
            }
            match question {
                ClassifierQuestion::Predicate { criteria, .. } => {
                    if let Some(Some(criteria)) = criteria {
                        for value in [&criteria.positive, &criteria.negative]
                            .into_iter()
                            .flatten()
                            .flatten()
                        {
                            value.visit_text(&mut |text| {
                                visitor(ClassifierTextKind::Instructions, index, text)
                            });
                        }
                    }
                }
                ClassifierQuestion::Choice { choices, .. } => {
                    for choice in choices {
                        match &choice.value {
                            ClassifierChoiceValue::String(value) => {
                                visitor(ClassifierTextKind::StringChoice, index, value)
                            }
                            ClassifierChoiceValue::Boolean(value) => visitor(
                                ClassifierTextKind::BooleanChoice,
                                index,
                                if *value { "true" } else { "false" },
                            ),
                        }
                        if let Some(description) = &choice.description {
                            description.visit_text(&mut |text| {
                                visitor(ClassifierTextKind::ChoiceDescription, index, text)
                            });
                        }
                    }
                }
                ClassifierQuestion::Score { levels, .. } => {
                    for level in levels {
                        visitor(ClassifierTextKind::LevelLabel, index, &level.label);
                        if let Some(criteria) = &level.criteria {
                            criteria.visit_text(&mut |text| {
                                visitor(ClassifierTextKind::LevelDescription, index, text)
                            });
                        }
                        if let Some(description) = &level.description {
                            description.visit_text(&mut |text| {
                                visitor(ClassifierTextKind::LevelDescription, index, text)
                            });
                        }
                    }
                }
            }
        }
    }

    /// Inline image count, separate from covered text.
    pub fn image_count(&self) -> u64 {
        let ClassifierInput::Messages(messages) = &self.input else {
            return 0;
        };
        messages
            .iter()
            .map(|message| match &message.content {
                ClassifierContent::Text(_) => 0,
                ClassifierContent::Parts(parts) => parts
                    .iter()
                    .filter(|part| matches!(part, ClassifierInputPart::InputImage { .. }))
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
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ClassifierInput {
    /// Plain text evidence.
    Text(String),
    /// Ordered user messages containing text or inline images.
    Messages(Vec<ClassifierMessage>),
    /// Structured evidence, retaining JSON field types.
    Structured(serde_json::Value),
}

/// The only role admitted by native Decisions input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifierRole {
    /// User-supplied evidence.
    User,
}

/// Optional native input message discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifierMessageType {
    /// An evidence message.
    Message,
}

/// A native user evidence message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierMessage {
    /// Native role, which can only be user.
    pub role: ClassifierRole,
    /// Optional native discriminator.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub message_type: Option<ClassifierMessageType>,
    /// Ordered evidence content.
    pub content: ClassifierContent,
}

/// Text or ordered native content parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClassifierContent {
    /// Plain text.
    Text(String),
    /// Text and inline images.
    Parts(Vec<ClassifierInputPart>),
}

/// Native Decisions image detail choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifierImageDetail {
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
pub enum ClassifierInputPart {
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
        detail: Option<Option<ClassifierImageDetail>>,
    },
}

/// A typed category value; boolean true differs from string "true".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClassifierChoiceValue {
    /// String category.
    String(String),
    /// Boolean category.
    Boolean(bool),
}

/// One supplied choice and its optional meaning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierChoice {
    /// Typed category value.
    pub value: ClassifierChoiceValue,
    /// Optional criteria for this choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<ClassifierText>,
}

/// One ordered rubric level, whose index starts at zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierLevel {
    /// Native level label.
    pub label: String,
    /// Native structured rubric retained independently of its display label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<ClassifierText>,
    /// Optional criteria for this level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<ClassifierText>,
}

/// A predicate, category choice or ordered score question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClassifierQuestion {
    /// Estimate whether a statement holds.
    Predicate {
        /// Native true/false rubric, when supplied.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "present_nullable")]
        criteria: Option<Option<ClassifierPredicateCriteria>>,
        /// Question instructions.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_text"
        )]
        instructions: Option<ClassifierText>,
        /// Optional answer identity; position remains authoritative.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Original System One question key, distinct from a native name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// Choose one supplied typed value.
    Choice {
        /// Question instructions.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_text"
        )]
        instructions: Option<ClassifierText>,
        /// Supplied categories, in native order.
        choices: Vec<ClassifierChoice>,
        /// Optional answer identity.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Original System One question key, distinct from a native name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// Evaluate an ordered rubric.
    Score {
        /// Question instructions.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_text"
        )]
        instructions: Option<ClassifierText>,
        /// Supplied rubric levels, lowest first.
        levels: Vec<ClassifierLevel>,
        /// Optional answer identity.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Original System One question key, distinct from a native name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
}

impl ClassifierQuestion {
    /// Client-owned System One identity; never model-visible instructions.
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::Predicate { key, .. } | Self::Choice { key, .. } | Self::Score { key, .. } => {
                key.as_deref()
            }
        }
    }

    /// Preserve a parsed map key independently of native positional names.
    pub fn set_key(&mut self, value: String) {
        match self {
            Self::Predicate { key, .. } | Self::Choice { key, .. } | Self::Score { key, .. } => {
                *key = Some(value)
            }
        }
    }
    /// Caller-supplied name, without replacing positional identity.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Predicate { name, .. } | Self::Choice { name, .. } | Self::Score { name, .. } => {
                name.as_deref()
            }
        }
    }

    /// Instructions defining this question's criteria.
    pub fn instructions(&self) -> Option<&ClassifierText> {
        match self {
            Self::Predicate { instructions, .. }
            | Self::Choice { instructions, .. }
            | Self::Score { instructions, .. } => instructions.as_ref(),
        }
    }
}

/// A native per-choice probability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifierChoiceProbability {
    /// Supplied typed choice value.
    pub value: ClassifierChoiceValue,
    /// Provider estimate in [0, 1].
    pub probability: f64,
    /// Same-wire additive fields, retained without interpretation.
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// A native per-level probability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifierScoreProbability {
    /// Zero-based rubric level index.
    pub value: u32,
    /// Supplied level label.
    pub label: ClassifierText,
    /// Provider estimate in [0, 1].
    pub probability: f64,
    /// Same-wire additive fields.
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// One ordered typed answer or refusal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClassifierAnswer {
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
        choice: ClassifierChoiceValue,
        /// Native probability distribution.
        probabilities: Vec<ClassifierChoiceProbability>,
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
        probabilities: Vec<ClassifierScoreProbability>,
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

impl ClassifierAnswer {
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
pub struct ClassifierResult {
    /// Actual provider-reported model.
    pub model: String,
    /// Actual upstream wire, retained across destination rendering.
    pub protocol: ApiProtocol,
    /// Answers in request order.
    pub answers: Vec<ClassifierAnswer>,
    /// Validated canonical usage, retaining the native usage object.
    pub usage: Usage,
    /// Same-wire additive envelope fields.
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Evidence retained when a complete provider response fails validation.
#[derive(Clone)]
pub struct ClassifierResponseFailure {
    /// Content-free structural diagnostic.
    pub message: String,
    /// Independently validated usage, even when answers are malformed.
    pub usage: Option<Box<Usage>>,
}

impl fmt::Display for ClassifierResponseFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl fmt::Debug for ClassifierResponseFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClassifierResponseFailure")
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

fn present_text<'de, D>(deserializer: D) -> std::result::Result<Option<ClassifierText>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    ClassifierText::deserialize(deserializer).map(Some)
}
