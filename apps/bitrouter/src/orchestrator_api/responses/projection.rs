//! Borrowed Responses wire views; output text is never copied into JSON values.

use bitrouter_orchestrator::core::protocol::{ToolExecute, VERSION};
use bitrouter_orchestrator::core::session::RunStatus;
use bitrouter_orchestrator::core::session::responses::{ResponseEvent, ResponseExchange};
use bitrouter_sdk::language_model::types::Content;
use serde::Serialize;
use serde::ser::{SerializeSeq, Serializer};
use serde_json::Value;
use std::collections::BTreeMap;

use super::super::{ApiError, ErrorCode};

#[derive(Serialize)]
pub(super) struct Initial {
    id: String,
    object: &'static str,
    created_at: u64,
    model: String,
    status: &'static str,
    output: [(); 0],
    usage: Option<()>,
    error: Option<()>,
    previous_response_id: Option<String>,
    bitrouter: InitialMetadata,
    #[serde(skip)]
    run_limit: Option<u64>,
}

#[derive(Serialize)]
struct InitialMetadata {
    version: u32,
    run_id: String,
    state_revision: u64,
}

impl Initial {
    pub(super) fn new(exchange: &ResponseExchange) -> Result<Self, ApiError> {
        Ok(Self {
            id: exchange.response_id.clone(),
            object: "response",
            created_at: created_at(exchange)?,
            model: exchange.model.clone(),
            status: "in_progress",
            output: [],
            usage: None,
            error: None,
            previous_response_id: exchange.previous_response_id.clone(),
            bitrouter: InitialMetadata {
                version: VERSION,
                run_id: exchange.run_id.clone(),
                state_revision: exchange.created_state_revision,
            },
            run_limit: exchange
                .input
                .as_ref()
                .and_then(|input| input.limits.as_ref())
                .map(|limits| limits.ephemeral_bytes),
        })
    }

    pub(super) fn run(&self, session_limit: u64) -> (&str, u64) {
        (
            &self.bitrouter.run_id,
            self.run_limit.unwrap_or(session_limit),
        )
    }
}

fn created_at(exchange: &ResponseExchange) -> Result<u64, ApiError> {
    exchange.created_at.ok_or_else(|| {
        ApiError::core(
            ErrorCode::UnsupportedCapability,
            "legacy response lacks durable HTTP projection metadata",
        )
    })
}

#[derive(Serialize)]
pub(super) struct Projection<'a> {
    id: &'a str,
    object: &'static str,
    created_at: u64,
    model: &'a str,
    pub(super) status: &'static str,
    pub(super) output: Items<'a>,
    previous_response_id: &'a Option<String>,
    usage: Option<()>,
    error: Option<Failure>,
    bitrouter: Metadata<'a>,
}

#[derive(Serialize)]
struct Failure {
    code: &'static str,
    message: &'static str,
}

#[derive(Serialize)]
struct Metadata<'a> {
    version: u32,
    run_id: &'a str,
    run_status: &'a Option<RunStatus>,
    state_revision: u64,
    pending_invocations: &'a BTreeMap<String, ToolExecute>,
    events: &'a [ResponseEvent],
}

impl<'a> Projection<'a> {
    pub(super) fn new(exchange: &'a ResponseExchange) -> Result<Self, ApiError> {
        let final_step = exchange.final_answer.as_ref().and_then(|answer| {
            exchange
                .output
                .iter()
                .rev()
                .find(|output| {
                    if output.agent_name != "/root" {
                        return false;
                    }
                    let mut remaining = answer.as_str();
                    for text in output.message.content.iter().filter_map(|part| match part {
                        Content::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    }) {
                        let Some(rest) = remaining.strip_prefix(text) else {
                            return false;
                        };
                        remaining = rest;
                    }
                    remaining.is_empty()
                })
                .map(|output| output.step_id.as_str())
        });
        let output = Items {
            exchange,
            final_step,
        };
        // Validate before sending HTTP headers. Serialization thereafter can
        // fail only because the body consumer or transport became unavailable.
        for item in output.iter() {
            item?;
        }
        let status = if exchange.completed_state_revision.is_none() {
            "in_progress"
        } else if matches!(
            exchange.run_status,
            Some(RunStatus::Failed | RunStatus::Cancelled | RunStatus::RecoveryRequired)
        ) {
            "failed"
        } else {
            "completed"
        };
        Ok(Self {
            id: &exchange.response_id,
            object: "response",
            created_at: created_at(exchange)?,
            model: &exchange.model,
            status,
            output,
            previous_response_id: &exchange.previous_response_id,
            usage: None,
            error: (status == "failed").then_some(Failure {
                code: "managed_run_stopped",
                message: "inspect the attributed run disposition",
            }),
            bitrouter: Metadata {
                version: VERSION,
                run_id: &exchange.run_id,
                run_status: &exchange.run_status,
                state_revision: exchange
                    .completed_state_revision
                    .unwrap_or(exchange.created_state_revision),
                pending_invocations: &exchange.pending,
                events: &exchange.events,
            },
        })
    }
}

#[derive(Clone, Copy, Serialize)]
pub(super) struct Agent<'a> {
    pub(super) agent_name: &'a str,
}

#[derive(Clone, Copy, Serialize)]
pub(super) struct Text<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    pub(super) text: &'a str,
    annotations: [(); 0],
}

impl<'a> Text<'a> {
    pub(super) fn new(text: &'a str) -> Self {
        Self {
            kind: "output_text",
            text,
            annotations: [],
        }
    }
}

// A compact sequence view avoids an allocated Vec for every message event.
#[derive(Clone, Copy)]
pub(super) struct ContentParts<'a>(pub(super) Option<Text<'a>>);
impl Serialize for ContentParts<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(usize::from(self.0.is_some())))?;
        if let Some(text) = self.0 {
            sequence.serialize_element(&text)?;
        }
        sequence.end()
    }
}

#[derive(Clone, Copy)]
pub(super) enum Arguments<'a> {
    Text(&'a str),
    Json(&'a Value),
}
impl Serialize for Arguments<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Text(text) => serializer.serialize_str(text),
            // serde_json streams Display fragments through JSON string escaping.
            // <https://docs.rs/serde/latest/serde/ser/trait.Serializer.html#method.collect_str>
            Self::Json(value) => serializer.collect_str(value),
        }
    }
}

#[derive(Clone, Copy, Serialize)]
struct Verification {
    verification: bool,
}

#[derive(Clone, Serialize)]
pub(super) struct Item<'a> {
    pub(super) id: String,
    #[serde(rename = "type")]
    pub(super) kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    pub(super) agent: Agent<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) content: Option<ContentParts<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) arguments: Option<Arguments<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bitrouter: Option<Verification>,
}

impl<'a> Item<'a> {
    fn new(id: String, agent_name: &'a str) -> Self {
        Self {
            id,
            kind: "",
            status: Some("completed"),
            agent: Agent { agent_name },
            role: None,
            phase: None,
            content: None,
            call_id: None,
            name: None,
            arguments: None,
            text: None,
            bitrouter: None,
        }
    }
    fn message(mut self, text: &'a str, final_answer: bool) -> Self {
        self.kind = "message";
        self.role = Some("assistant");
        self.phase = Some(if final_answer {
            "final_answer"
        } else {
            "commentary"
        });
        self.content = Some(ContentParts(Some(Text::new(text))));
        self
    }
    pub(super) fn added(&self) -> Self {
        let mut initial = self.clone();
        initial.status = Some("in_progress");
        if self.kind == "message" {
            initial.content = Some(ContentParts(None));
        }
        if self.kind == "function_call" {
            initial.arguments = Some(Arguments::Text(""));
        }
        initial
    }
}

pub(super) struct Items<'a> {
    exchange: &'a ResponseExchange,
    final_step: Option<&'a str>,
}
impl<'a> Items<'a> {
    pub(super) fn iter(&self) -> impl Iterator<Item = Result<Item<'a>, ApiError>> + '_ {
        let outputs = self.exchange.output.iter().flat_map(move |output| {
            output
                .message
                .content
                .iter()
                .enumerate()
                .map(move |(index, part)| {
                    let mut item =
                        Item::new(format!("{}_{}", output.step_id, index), &output.agent_name);
                    match part {
                        Content::Text { text, .. } => {
                            item =
                                item.message(text, self.final_step == Some(output.step_id.as_str()))
                        }
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                            provider_executed,
                            ..
                        } => {
                            item.kind = if *provider_executed {
                                "bitrouter.provider_call"
                            } else if bitrouter_orchestrator::core::protocol::COLLABORATION_TOOLS
                                .contains(&name.as_str())
                            {
                                "bitrouter.collaboration_call"
                            } else {
                                "function_call"
                            };
                            item.call_id = Some(output.call_ids.get(id).ok_or_else(|| {
                                ApiError::core(
                                    ErrorCode::CheckpointConflict,
                                    "response call has no public mapping",
                                )
                            })?);
                            item.name = Some(name);
                            item.arguments = Some(Arguments::Text(arguments));
                        }
                        Content::Reasoning { text, .. } => {
                            item.kind = "bitrouter.reasoning";
                            item.status = None;
                            item.text = Some(text);
                        }
                        _ => {
                            return Err(ApiError::core(
                                ErrorCode::UnsupportedCapability,
                                "response contains an unsupported output part",
                            ));
                        }
                    }
                    Ok(item)
                })
        });
        let verification = self
            .exchange
            .pending
            .iter()
            .filter(|(_, command)| {
                command.verification
                    && command.response_id.as_ref() == Some(&self.exchange.response_id)
            })
            .map(|(call_id, command)| {
                let mut item = Item::new(format!("function_{}", command.invocation_id), "/root");
                item.kind = "function_call";
                item.call_id = Some(call_id);
                item.name = Some(&command.tool);
                item.arguments = Some(Arguments::Json(&command.arguments));
                item.bitrouter = Some(Verification { verification: true });
                Ok(item)
            });
        let final_answer = self
            .exchange
            .final_answer
            .as_ref()
            .filter(|_| self.final_step.is_none())
            .map(|answer| {
                Ok(
                    Item::new(format!("{}_final", self.exchange.response_id), "/root")
                        .message(answer, true),
                )
            });
        outputs.chain(verification).chain(final_answer)
    }
}
impl Serialize for Items<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        for item in self.iter() {
            sequence
                .serialize_element(&item.map_err(|error| serde::ser::Error::custom(error.0))?)?;
        }
        sequence.end()
    }
}
