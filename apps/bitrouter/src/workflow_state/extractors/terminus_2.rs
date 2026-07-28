//! Workflow-state extraction for Harbor's Terminus 2 reference agent.
//!
//! Terminus 2 emits text actions in a bounded JSON or XML schema and executes
//! their `commands[].keystrokes` through tmux. This parser only classifies that
//! text; it never evaluates or executes command content.
//! Official implementation and prompt contract:
//! <https://github.com/harbor-framework/harbor/tree/main/src/harbor/agents/terminus_2>

use bitrouter_sdk::language_model::types::{Content, Prompt, Role};

use crate::workflow_state::extractors::generic::GenericPromptExtractor;
use crate::workflow_state::extractors::{ExtractorInput, WorkflowStateExtractor};
use crate::workflow_state::ir::{
    Evidence, EvidenceLevel, HarnessId, RecoverySignal, RequirementLevel, ToolDensity,
    WorkflowStateIR, WorkflowStateKind,
};

const MAX_ACTION_BYTES: usize = 256 * 1024;

pub struct Terminus2Extractor;

impl WorkflowStateExtractor for Terminus2Extractor {
    fn detect(
        &self,
        input: &ExtractorInput<'_>,
    ) -> Option<crate::workflow_state::extractors::TraceAdapterMatch> {
        has_terminus_2_prompt_contract(input.prompt).then_some(
            crate::workflow_state::extractors::TraceAdapterMatch {
                source: HarnessId::Terminus2,
                confidence: 0.95,
                evidence_kind: "terminus_2_prompt_contract",
                native: true,
            },
        )
    }

    fn extract(&self, input: &ExtractorInput<'_>) -> WorkflowStateIR {
        let mut ir = GenericPromptExtractor.extract(&ExtractorInput {
            harness_hint: Some(HarnessId::Terminus2),
            protocol_hint: input.protocol_hint.clone(),
            headers: input.headers,
            raw_body: input.raw_body,
            prompt: input.prompt,
        });
        let Some(action) = latest_assistant_action(input.prompt) else {
            return ir;
        };

        ir.evidence.push(Evidence {
            kind: "terminus_2_action_format".to_string(),
            value: action.format.as_str().to_string(),
            confidence: 0.98,
            level: EvidenceLevel::Observed,
        });
        if action.task_complete {
            ir.state_kind = WorkflowStateKind::Finalization;
            return ir;
        }
        if action.commands.is_empty() {
            return ir;
        }

        let intent = classify_commands(&action.commands);
        ir.state_kind = if ir.recovery_signal == RecoverySignal::LikelyRecovery {
            WorkflowStateKind::Recovery
        } else {
            match intent {
                CommandIntent::Test => WorkflowStateKind::Test,
                CommandIntent::Edit => WorkflowStateKind::Edit,
                CommandIntent::Review => WorkflowStateKind::Review,
                CommandIntent::Other => WorkflowStateKind::ToolFollowup,
            }
        };
        ir.last_tool_name = Some("tmux_shell".to_string());
        ir.tool_density = if action.commands.len() == 1 {
            ToolDensity::Low
        } else {
            ToolDensity::High
        };
        ir.capability_constraints.tool_reliability = RequirementLevel::High;
        if !ir
            .capability_constraints
            .compatibility
            .iter()
            .any(|value| value == "requires_terminal_interaction")
        {
            ir.capability_constraints
                .compatibility
                .push("requires_terminal_interaction".to_string());
        }
        ir.evidence.push(Evidence {
            kind: "terminus_2_command_intent".to_string(),
            value: intent.as_str().to_string(),
            confidence: 0.9,
            level: EvidenceLevel::Inferred,
        });
        ir
    }
}

pub(crate) fn has_terminus_2_prompt_contract(prompt: &Prompt) -> bool {
    prompt
        .system
        .iter()
        .map(String::as_str)
        .chain(
            prompt
                .messages
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(|content| match content {
                    Content::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                }),
        )
        .any(|text| {
            let normalized = text.to_ascii_lowercase();
            normalized.contains(
                "you are an ai assistant tasked with solving command-line tasks in a linux environment",
            ) && (normalized.contains("format your response as json")
                || normalized.contains("format your response as xml"))
                && normalized.contains("commands")
                && normalized.contains("task_complete")
        })
}

struct ParsedAction {
    commands: Vec<String>,
    task_complete: bool,
    format: ActionFormat,
}

#[derive(Debug, Clone)]
pub(crate) struct TerminusSessionIdentity {
    pub parent_session_id: String,
    pub role: Option<crate::workflow_state::ir::AgentRole>,
    pub context_epoch: Option<u32>,
}

pub(crate) fn session_id(input: &ExtractorInput<'_>) -> Option<String> {
    input
        .headers
        .get("x-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            input
                .raw_body
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
        })
}

pub(crate) fn parse_session_identity(session_id: &str) -> TerminusSessionIdentity {
    use crate::workflow_state::ir::AgentRole;

    if let Some(marker) = session_id.rfind("-summarization-") {
        let parent = &session_id[..marker];
        let suffix = &session_id[marker + "-summarization-".len()..];
        if !parent.is_empty()
            && let Some((epoch, role)) = suffix.split_once('-')
            && let Ok(context_epoch) = epoch.parse::<u32>()
        {
            let role = match role {
                "summary" => Some(AgentRole::Summary),
                "questions" => Some(AgentRole::Questions),
                "answers" => Some(AgentRole::Answers),
                _ => None,
            };
            if role.is_some() {
                return TerminusSessionIdentity {
                    parent_session_id: parent.to_string(),
                    role,
                    context_epoch: Some(context_epoch),
                };
            }
        }
    }
    if let Some(marker) = session_id.rfind("-cont-") {
        let parent = &session_id[..marker];
        let epoch = &session_id[marker + "-cont-".len()..];
        if !parent.is_empty()
            && let Ok(context_epoch) = epoch.parse::<u32>()
        {
            return TerminusSessionIdentity {
                parent_session_id: parent.to_string(),
                role: Some(AgentRole::Main),
                context_epoch: Some(context_epoch),
            };
        }
    }
    TerminusSessionIdentity {
        parent_session_id: session_id.to_string(),
        role: None,
        context_epoch: None,
    }
}

pub(crate) fn infer_role(prompt: &Prompt) -> crate::workflow_state::ir::AgentRole {
    use crate::workflow_state::ir::AgentRole;

    let latest_user = prompt
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let normalized = latest_user.to_ascii_lowercase();
    if normalized.contains("you are about to hand off your work to another ai agent") {
        AgentRole::Summary
    } else if normalized.contains("you are picking up work from a previous ai agent on this task") {
        AgentRole::Questions
    } else if normalized.contains("the next agent has a few questions for you") {
        AgentRole::Answers
    } else if normalized.trim().is_empty() {
        AgentRole::Unknown
    } else {
        AgentRole::Main
    }
}

#[derive(Clone, Copy)]
enum ActionFormat {
    Json,
    Xml,
}

impl ActionFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Xml => "xml",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommandIntent {
    Test,
    Edit,
    Review,
    Other,
}

impl CommandIntent {
    fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Edit => "edit",
            Self::Review => "review",
            Self::Other => "other",
        }
    }
}

fn latest_assistant_action(prompt: &Prompt) -> Option<ParsedAction> {
    prompt
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .and_then(|message| {
            message
                .content
                .iter()
                .rev()
                .find_map(|content| match content {
                    Content::Text { text, .. } => {
                        parse_json_action(text).or_else(|| parse_xml_action(text))
                    }
                    _ => None,
                })
        })
}

fn parse_json_action(text: &str) -> Option<ParsedAction> {
    let trimmed = text.trim();
    if trimmed.len() > MAX_ACTION_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok().or_else(|| {
        let start = trimmed.find('{')?;
        let end = trimmed.rfind('}')?;
        (start < end)
            .then(|| &trimmed[start..=end])
            .and_then(|candidate| serde_json::from_str(candidate).ok())
    })?;
    let commands = value
        .get("commands")?
        .as_array()?
        .iter()
        .filter_map(|command| {
            command
                .get("keystrokes")
                .and_then(serde_json::Value::as_str)
        })
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let task_complete = value
        .get("task_complete")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    Some(ParsedAction {
        commands,
        task_complete,
        format: ActionFormat::Json,
    })
}

fn parse_xml_action(text: &str) -> Option<ParsedAction> {
    let trimmed = text.trim();
    if trimmed.len() > MAX_ACTION_BYTES || !trimmed.contains("<response") {
        return None;
    }
    let commands = xml_element_values(trimmed, "keystrokes");
    let task_complete = xml_element_values(trimmed, "task_complete")
        .into_iter()
        .any(|value| value.trim().eq_ignore_ascii_case("true"));
    if commands.is_empty() && !trimmed.contains("<task_complete") {
        return None;
    }
    Some(ParsedAction {
        commands,
        task_complete,
        format: ActionFormat::Xml,
    })
}

fn xml_element_values(text: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut values = Vec::new();
    let mut rest = text;
    while let Some(open_start) = rest.find(&open) {
        let after_open = &rest[open_start + open.len()..];
        let Some(open_end) = after_open.find('>') else {
            break;
        };
        let content = &after_open[open_end + 1..];
        let Some(close_start) = content.find(&close) else {
            break;
        };
        values.push(content[..close_start].to_string());
        rest = &content[close_start + close.len()..];
    }
    values
}

fn classify_commands(commands: &[String]) -> CommandIntent {
    if commands.iter().any(|command| is_test_command(command)) {
        return CommandIntent::Test;
    }
    if commands.iter().any(|command| is_edit_command(command)) {
        return CommandIntent::Edit;
    }
    if commands.iter().any(|command| is_review_command(command)) {
        return CommandIntent::Review;
    }
    CommandIntent::Other
}

fn is_test_command(command: &str) -> bool {
    command_segments(command).iter().any(|normalized| {
        [
            "cargo test",
            "pytest",
            "python -m pytest",
            "python3 -m pytest",
            "go test",
            "npm test",
            "pnpm test",
            "yarn test",
            "ctest",
            "make test",
        ]
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
            || normalized.contains("run-tests")
    })
}

fn command_segments(command: &str) -> Vec<String> {
    command
        .to_ascii_lowercase()
        .replace("&&", ";")
        .replace("||", ";")
        .lines()
        .flat_map(|line| line.split(';'))
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn is_edit_command(command: &str) -> bool {
    command_segments(command).iter().any(|normalized| {
        [
            "apply_patch",
            "sed -i",
            "perl -pi",
            "tee ",
            "touch ",
            "mkdir ",
            "rm ",
            "mv ",
            "cp ",
            "chmod ",
            "git add",
            "git commit",
        ]
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
            || normalized.contains(" > ")
            || normalized.contains(" >> ")
    })
}

fn is_review_command(command: &str) -> bool {
    command_segments(command).iter().any(|normalized| {
        [
            "git diff",
            "git status",
            "ls",
            "find ",
            "rg ",
            "grep ",
            "cat ",
            "head ",
            "tail ",
            "pwd",
            "sed -n",
        ]
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
    })
}
