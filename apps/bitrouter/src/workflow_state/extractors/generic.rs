use bitrouter_sdk::language_model::types::{
    Content, Prompt, Role, ToolResultContentPart, ToolResultOutput,
};

use crate::workflow_state::extractors::{ExtractorInput, WorkflowStateExtractor};
use crate::workflow_state::ir::{
    CapabilityConstraints, ContextSizeBucket, Evidence, EvidenceLevel, HarnessId, RecoverySignal,
    RequirementLevel, ToolDensity, WorkflowStateIR, WorkflowStateKind,
};
use crate::workflow_state::session::resolve_session_signal;

pub struct GenericPromptExtractor;

const GUARDED_TOOL_TRAJECTORY_ASSISTANT_TURNS: usize = 8;

impl WorkflowStateExtractor for GenericPromptExtractor {
    fn extract(&self, input: &ExtractorInput<'_>) -> WorkflowStateIR {
        let prompt = input.prompt;
        let (mut state_kind, last_tool_name, state_evidence) = classify_state(prompt);
        let context_size = context_size_bucket(prompt);
        let tool_density = tool_density(prompt);
        let recovery_signal = recovery_signal(prompt);
        if recovery_signal == RecoverySignal::LikelyRecovery
            && matches!(state_kind, WorkflowStateKind::ToolFollowup)
        {
            state_kind = WorkflowStateKind::Recovery;
        }
        let mut capability_constraints = CapabilityConstraints {
            tool_reliability: if tool_density == ToolDensity::None {
                RequirementLevel::Low
            } else {
                RequirementLevel::High
            },
            code_reasoning: if matches!(
                state_kind,
                WorkflowStateKind::Debug | WorkflowStateKind::Review | WorkflowStateKind::Recovery
            ) || recovery_signal == RecoverySignal::LikelyRecovery
            {
                RequirementLevel::Medium
            } else {
                RequirementLevel::Low
            },
            context_pressure: match context_size {
                ContextSizeBucket::Large => RequirementLevel::High,
                ContextSizeBucket::Medium => RequirementLevel::Medium,
                ContextSizeBucket::Small => RequirementLevel::Low,
                ContextSizeBucket::Unknown => RequirementLevel::Unknown,
            },
            latency_sensitivity: RequirementLevel::Low,
            expected_redo_penalty: if recovery_signal == RecoverySignal::LikelyRecovery {
                RequirementLevel::High
            } else {
                RequirementLevel::Medium
            },
            output_precision: RequirementLevel::Medium,
            compatibility: Vec::new(),
        };
        if tool_density != ToolDensity::None {
            capability_constraints
                .compatibility
                .push("requires_structured_tools".to_string());
        }

        let resolved_session = resolve_session_signal(input);
        let mut evidence = vec![state_evidence];
        evidence.extend(resolved_session.evidence);
        evidence.push(Evidence {
            kind: "context_size".to_string(),
            value: format!("{context_size:?}"),
            confidence: 0.65,
            level: EvidenceLevel::Inferred,
        });
        if recovery_signal == RecoverySignal::LikelyRecovery {
            evidence.push(Evidence {
                kind: "recovery_marker".to_string(),
                value: "recent observation reports execution failure".to_string(),
                confidence: 0.9,
                level: EvidenceLevel::Observed,
            });
        }

        let mut ir = WorkflowStateIR {
            harness_id: input.harness_hint.clone().unwrap_or(HarnessId::Generic),
            protocol: input.protocol_hint.clone(),
            state_kind,
            active_workflow: None,
            subagent_role: None,
            last_tool_name,
            tool_density,
            context_size,
            recovery_signal,
            capability_constraints,
            session: resolved_session.signal,
            identity: Default::default(),
            confidence: 0.7,
            evidence,
        };
        apply_trajectory_pressure(prompt, &mut ir);
        ir
    }
}

pub(crate) fn apply_trajectory_pressure(prompt: &Prompt, ir: &mut WorkflowStateIR) {
    let observed_structured_action = prompt
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|content| {
            matches!(
                content,
                Content::ToolCall { .. } | Content::ToolResult { .. }
            )
        });
    let observed_native_action = ir
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "terminus_2_action_format");
    if !observed_structured_action && !observed_native_action {
        return;
    }
    let assistant_turns = prompt
        .messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .count();
    if assistant_turns < GUARDED_TOOL_TRAJECTORY_ASSISTANT_TURNS {
        return;
    }
    ir.capability_constraints.expected_redo_penalty = RequirementLevel::High;
    if !ir
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "trajectory_pressure")
    {
        ir.evidence.push(Evidence {
            kind: "trajectory_pressure".to_string(),
            value: format!("assistant_turns:{assistant_turns}"),
            confidence: 0.9,
            level: EvidenceLevel::Observed,
        });
    }
}

fn classify_state(prompt: &Prompt) -> (WorkflowStateKind, Option<String>, Evidence) {
    for message in prompt.messages.iter().rev() {
        if message.role != Role::Assistant {
            continue;
        }
        if let Some((state_kind, name)) = message.content.iter().rev().find_map(tool_call_state) {
            return (
                state_kind,
                Some(name.to_string()),
                Evidence {
                    kind: "last_assistant_tool_call".to_string(),
                    value: name.to_string(),
                    confidence: 0.9,
                    level: EvidenceLevel::Observed,
                },
            );
        }
        return (
            WorkflowStateKind::Planning,
            None,
            Evidence {
                kind: "last_assistant_text_turn".to_string(),
                value: "assistant turn without tool call".to_string(),
                confidence: 0.55,
                level: EvidenceLevel::Inferred,
            },
        );
    }
    (
        WorkflowStateKind::Opening,
        None,
        Evidence {
            kind: "no_assistant_turn".to_string(),
            value: "opening".to_string(),
            confidence: 0.95,
            level: EvidenceLevel::Observed,
        },
    )
}

fn tool_call_state(content: &Content) -> Option<(WorkflowStateKind, &str)> {
    match content {
        Content::ToolCall {
            name, arguments, ..
        } => Some((tool_call_intent(name, arguments), name.as_str())),
        _ => None,
    }
}

fn tool_call_intent(name: &str, arguments: &str) -> WorkflowStateKind {
    match name {
        "apply_patch" | "Edit" => WorkflowStateKind::Edit,
        "Bash" if canonical_test_command(arguments, "command") => WorkflowStateKind::Test,
        "bash" | "shell" | "terminal" | "exec_command"
            if canonical_test_command(arguments, "cmd") =>
        {
            WorkflowStateKind::Test
        }
        _ => WorkflowStateKind::ToolFollowup,
    }
}

fn canonical_test_command(arguments: &str, expected_field: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    let command_fields = ["command", "cmd"]
        .into_iter()
        .filter(|field| object.contains_key(*field))
        .collect::<Vec<_>>();
    if command_fields != [expected_field] {
        return false;
    }
    let Some(command) = object
        .get(expected_field)
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    if !is_ascii_shell_input(command) {
        return false;
    }
    let command = command.trim_matches([' ', '\t']);
    if command.is_empty() || contains_shell_syntax(command) {
        return false;
    }
    is_canonical_test_command(command)
}

fn is_ascii_shell_input(command: &str) -> bool {
    command.is_ascii()
        && !command
            .bytes()
            .any(|byte| byte.is_ascii_control() && byte != b'\t')
}

fn contains_shell_syntax(command: &str) -> bool {
    command.chars().any(|character| {
        matches!(
            character,
            '\n' | '\r'
                | ';'
                | '&'
                | '|'
                | '#'
                | '\''
                | '"'
                | '`'
                | '$'
                | '<'
                | '>'
                | '\\'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '*'
                | '?'
                | '!'
                | '~'
        )
    })
}

fn is_canonical_test_command(command: &str) -> bool {
    let mut tokens = command.split([' ', '\t']).filter(|token| !token.is_empty());
    match tokens.next() {
        Some("cargo") | Some("go") | Some("npm") | Some("pnpm") | Some("yarn") | Some("make") => {
            matches!(tokens.next(), Some("test"))
        }
        Some("python") | Some("python3") => {
            matches!(tokens.next(), Some("-m")) && matches!(tokens.next(), Some("pytest"))
        }
        Some("pytest") | Some("ctest") => true,
        _ => false,
    }
}

fn tool_density(prompt: &Prompt) -> ToolDensity {
    let declared_tools = prompt.tools.len();
    let tool_events = prompt
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|c| matches!(c, Content::ToolCall { .. } | Content::ToolResult { .. }))
        .count();
    match declared_tools + tool_events {
        0 => ToolDensity::None,
        1..=2 => ToolDensity::Low,
        _ => ToolDensity::High,
    }
}

fn context_size_bucket(prompt: &Prompt) -> ContextSizeBucket {
    let size = prompt
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .map(content_size)
        .sum::<usize>()
        + prompt.system.as_ref().map_or(0, |s| s.len());
    match size {
        0..=10_000 => ContextSizeBucket::Small,
        10_001..=50_000 => ContextSizeBucket::Medium,
        _ => ContextSizeBucket::Large,
    }
}

fn content_size(content: &Content) -> usize {
    match content {
        Content::Text { text, .. } | Content::Reasoning { text, .. } => text.len(),
        Content::ToolCall {
            name, arguments, ..
        } => name.len() + arguments.len(),
        Content::ToolResult { output, .. } => tool_result_text(output).len(),
        other => serde_json::to_string(other).map_or(0, |s| s.len()),
    }
}

fn recovery_signal(prompt: &Prompt) -> RecoverySignal {
    const RECOVERY_OBSERVATION_WINDOW: usize = 3;
    let structured_failure = prompt
        .messages
        .iter()
        .rev()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .take(RECOVERY_OBSERVATION_WINDOW)
        .any(tool_result_reports_failure);
    let transcript_failure = recent_terminal_outputs(prompt, RECOVERY_OBSERVATION_WINDOW)
        .into_iter()
        .any(plain_output_reports_failure);
    if structured_failure || transcript_failure {
        RecoverySignal::LikelyRecovery
    } else {
        RecoverySignal::None
    }
}

fn tool_result_reports_failure(output: &ToolResultOutput) -> bool {
    match output {
        ToolResultOutput::ErrorText { .. }
        | ToolResultOutput::ErrorJson { .. }
        | ToolResultOutput::ExecutionDenied { .. } => true,
        ToolResultOutput::Text { value } => plain_output_reports_failure(value),
        ToolResultOutput::Json { value } => plain_output_reports_failure(&value.to_string()),
        ToolResultOutput::Content { value } => value
            .iter()
            .filter_map(tool_part_text)
            .any(|text| plain_output_reports_failure(&text)),
    }
}

fn recent_terminal_outputs(prompt: &Prompt, maximum: usize) -> Vec<&str> {
    let mut outputs = Vec::new();
    for (index, message) in prompt.messages.iter().enumerate().rev() {
        if outputs.len() >= maximum {
            break;
        }
        if message.role != Role::User
            || !prompt.messages[..index]
                .iter()
                .any(|prior| prior.role == Role::Assistant)
        {
            continue;
        }
        if let Some(output) = terminal_output(message) {
            outputs.push(output);
        }
    }
    outputs
}

fn terminal_output(message: &bitrouter_sdk::language_model::types::Message) -> Option<&str> {
    const MARKER: &str = "New Terminal Output:";
    let mut plain_result = None;
    for content in message.content.iter().rev() {
        let Content::Text { text, .. } = content else {
            continue;
        };
        if let Some((_, output)) = text.rsplit_once(MARKER) {
            return Some(output);
        }
        plain_result.get_or_insert(text.as_str());
    }
    plain_result
}

fn plain_output_reports_failure(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        if line.is_empty() || is_shell_echo_line(line) {
            return false;
        }
        let lower = line.to_ascii_lowercase();
        explicit_nonzero_exit(&lower)
            || lower.starts_with("error:")
            || lower.starts_with("traceback (most recent call last):")
            || (lower.starts_with("thread '") && lower.contains(" panicked at "))
            || lower.ends_with(": command not found")
            || lower.ends_with(": no such file or directory")
            || lower.starts_with("failed ")
            || lower.starts_with("failed:")
    })
}

fn explicit_nonzero_exit(line: &str) -> bool {
    [
        "process exited with code",
        "command exited with code",
        "exit code",
        "exit_code",
    ]
    .into_iter()
    .filter_map(|marker| line.find(marker).map(|index| &line[index + marker.len()..]))
    .any(|suffix| {
        let suffix = suffix.trim_start_matches([' ', '\t', ':', '=', '"', '\'']);
        let number = suffix
            .chars()
            .take_while(|character| character.is_ascii_digit() || *character == '-')
            .collect::<String>();
        number.parse::<i64>().is_ok_and(|code| code != 0)
    })
}

fn is_shell_echo_line(line: &str) -> bool {
    if line.starts_with('>') {
        return true;
    }
    ["# ", "$ "].into_iter().any(|delimiter| {
        line.find(delimiter).is_some_and(|index| {
            let prefix = &line[..index];
            prefix.contains('@') && prefix.contains(':')
        })
    })
}

fn tool_result_text(output: &ToolResultOutput) -> String {
    match output {
        ToolResultOutput::Text { value }
        | ToolResultOutput::ErrorText { value }
        | ToolResultOutput::ExecutionDenied {
            reason: Some(value),
        } => value.clone(),
        ToolResultOutput::ExecutionDenied { reason: None } => String::new(),
        ToolResultOutput::Json { value } | ToolResultOutput::ErrorJson { value } => {
            value.to_string()
        }
        ToolResultOutput::Content { value } => value.iter().filter_map(tool_part_text).collect(),
    }
}

fn tool_part_text(part: &ToolResultContentPart) -> Option<String> {
    match part {
        ToolResultContentPart::Text { text } => Some(text.clone()),
        other => serde_json::to_string(other).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bitrouter_sdk::HeaderMap;
    use bitrouter_sdk::language_model::types::{
        Content, GenerationParams, Message, Prompt, ProviderMetadata, Role, Tool, ToolResultOutput,
    };

    use crate::workflow_state::ir::{
        ContextSizeBucket, ProtocolKind, RecoverySignal, RequirementLevel, ToolDensity,
        WorkflowStateKind,
    };

    fn prompt(messages: Vec<Message>, tools: Vec<Tool>) -> Prompt {
        Prompt {
            model: "inbound".to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages,
            tools,
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn user(text: &str) -> Message {
        Message::text(Role::User, text)
    }

    fn assistant_calls(tool: &str) -> Message {
        assistant_call(tool, "{}")
    }

    fn assistant_call(tool: &str, arguments: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall {
                id: format!("call_{tool}"),
                name: tool.to_string(),
                arguments: arguments.to_string(),
                provider_executed: false,
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            }],
        }
    }

    fn tool_result(text: &str) -> Message {
        Message {
            role: Role::Tool,
            content: vec![Content::ToolResult {
                call_id: "call_bash".to_string(),
                tool_name: Some("bash".to_string()),
                output: ToolResultOutput::Text {
                    value: text.to_string(),
                },
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            }],
        }
    }

    fn tool_error_result(text: &str) -> Message {
        Message {
            role: Role::Tool,
            content: vec![Content::ToolResult {
                call_id: "call_bash".to_string(),
                tool_name: Some("bash".to_string()),
                output: ToolResultOutput::ErrorText {
                    value: text.to_string(),
                },
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            }],
        }
    }

    fn extract(prompt: &Prompt) -> crate::workflow_state::ir::WorkflowStateIR {
        let headers = HeaderMap::new();
        let raw_body = serde_json::json!({});
        GenericPromptExtractor.extract(&ExtractorInput {
            harness_hint: None,
            protocol_hint: ProtocolKind::ChatCompletions,
            headers: &headers,
            raw_body: &raw_body,
            prompt,
        })
    }

    #[test]
    fn generic_extracts_opening_from_no_assistant_turn() {
        let prompt = prompt(vec![user("start")], Vec::new());
        let ir = extract(&prompt);
        assert_eq!(ir.state_kind, WorkflowStateKind::Opening);
        assert_eq!(ir.tool_density, ToolDensity::None);
    }

    #[test]
    fn generic_extracts_tool_followup_from_last_tool_call() {
        let prompt = prompt(vec![user("run tests"), assistant_calls("bash")], Vec::new());
        let ir = extract(&prompt);
        assert_eq!(ir.state_kind, WorkflowStateKind::ToolFollowup);
        assert_eq!(ir.last_tool_name.as_deref(), Some("bash"));
    }

    #[test]
    fn generic_tool_intent_maps_verified_edit_and_test_calls_without_source_data() {
        let edit = extract(&prompt(
            vec![user("continue"), assistant_call("apply_patch", "{}")],
            Vec::new(),
        ));
        assert_eq!(edit.state_kind, WorkflowStateKind::Edit);

        let test = extract(&prompt(
            vec![
                user("continue"),
                assistant_call("bash", r#"{"cmd":"cargo test -p bitrouter"}"#),
            ],
            Vec::new(),
        ));
        assert_eq!(test.state_kind, WorkflowStateKind::Test);
    }

    #[test]
    fn generic_test_intent_requires_a_documented_command_argument_at_command_position() {
        let claude_bash = extract(&prompt(
            vec![
                user("continue"),
                assistant_call("Bash", r#"{"command":"cargo test -p bitrouter"}"#),
            ],
            Vec::new(),
        ));
        assert_eq!(claude_bash.state_kind, WorkflowStateKind::Test);

        let pytest = extract(&prompt(
            vec![
                user("continue"),
                assistant_call("Bash", r#"{"command":"pytest -q"}"#),
            ],
            Vec::new(),
        ));
        assert_eq!(pytest.state_kind, WorkflowStateKind::Test);

        for (tool, arguments) in [
            ("bash", r#"{"cmd":"echo hello","description":"cargo test"}"#),
            ("Bash", r#"{"command":"echo cargo test"}"#),
            ("Bash", r##"{"command":"# cargo test"}"##),
            ("Bash", r#"{"command":"\"cargo test\""}"#),
            ("Bash", r#"{"command":"cargo test","cmd":"rm -rf scratch"}"#),
            ("Bash", r#"{"metadata":"cargo test"}"#),
            ("Bash", "{not-json"),
            ("Bash", "[]"),
            ("bash", r#"{"cmd":"pwd","metadata":{"tool_name":"Edit"}}"#),
            ("not_a_shell", r#"{"command":"cargo test"}"#),
            ("Bash", r##"{"command":"# ignored; cargo test"}"##),
            ("Bash", r##"{"command":"# ignored && cargo test"}"##),
            ("Bash", r#"{"command":"echo hello && cargo test"}"#),
            ("Bash", r#"{"command":"cargo test \"quoted; data\""}"#),
            ("Bash", "{\"command\":\"cat <<'EOF'\\ncargo test\\nEOF\"}"),
            ("Bash", r#"{"command":"cargo test | cat"}"#),
            ("Bash", r#"{"command":"cargo test > results.txt"}"#),
            ("Bash", r#"{"command":"cargo test $(date)"}"#),
            ("Bash", r#"{"command":"cargo test `date`"}"#),
            ("Bash", r#"{"command":"cargo test\r"}"#),
            ("Bash", "{\"command\":\"cargo\\u00a0test\"}"),
            ("Bash", r#"{"command":"cargo test\u000b"}"#),
            ("Bash", r#"{"command":"cargo test\u000c"}"#),
        ] {
            let ir = extract(&prompt(
                vec![user("continue"), assistant_call(tool, arguments)],
                Vec::new(),
            ));
            assert_eq!(
                ir.state_kind,
                WorkflowStateKind::ToolFollowup,
                "{tool} {arguments} must not select the economy test route"
            );
        }
    }

    #[test]
    fn generic_marks_recovery_when_recent_tool_result_contains_error() {
        let prompt = prompt(
            vec![
                user("run tests"),
                assistant_calls("bash"),
                tool_result("error: test failed with exit code 1"),
            ],
            Vec::new(),
        );
        let ir = extract(&prompt);
        assert_eq!(ir.recovery_signal, RecoverySignal::LikelyRecovery);
    }

    #[test]
    fn generic_keeps_recovery_guard_through_two_successful_tool_observations() {
        let prompt = prompt(
            vec![
                user("run tests"),
                assistant_calls("bash"),
                tool_error_result("operation could not complete"),
                assistant_calls("bash"),
                tool_result("first repair command completed"),
                assistant_calls("bash"),
                tool_result("second repair command completed"),
                assistant_calls("bash"),
            ],
            Vec::new(),
        );

        let ir = extract(&prompt);

        assert_eq!(ir.recovery_signal, RecoverySignal::LikelyRecovery);
        assert_eq!(ir.state_kind, WorkflowStateKind::Recovery);
    }

    #[test]
    fn generic_keeps_recovery_guard_through_two_successful_terminal_observations() {
        let prompt = prompt(
            vec![
                user("run tests"),
                assistant_calls("bash"),
                user("New Terminal Output:\nerror: test command failed"),
                assistant_calls("bash"),
                user("New Terminal Output:\nfirst repair command completed"),
                assistant_calls("bash"),
                user("New Terminal Output:\nsecond repair command completed"),
                assistant_calls("bash"),
            ],
            Vec::new(),
        );

        let ir = extract(&prompt);

        assert_eq!(ir.recovery_signal, RecoverySignal::LikelyRecovery);
        assert_eq!(ir.state_kind, WorkflowStateKind::Recovery);
    }

    #[test]
    fn generic_releases_structured_recovery_after_three_successful_observations() {
        let prompt = prompt(
            vec![
                user("run tests"),
                assistant_calls("bash"),
                tool_error_result("operation could not complete"),
                assistant_calls("bash"),
                tool_result("first repair command completed"),
                assistant_calls("bash"),
                tool_result("second repair command completed"),
                assistant_calls("bash"),
                tool_result("third repair command completed"),
                assistant_calls("bash"),
            ],
            Vec::new(),
        );

        let ir = extract(&prompt);

        assert_eq!(ir.recovery_signal, RecoverySignal::None);
        assert_eq!(ir.state_kind, WorkflowStateKind::ToolFollowup);
    }

    #[test]
    fn generic_releases_terminal_recovery_after_three_successful_observations() {
        let prompt = prompt(
            vec![
                user("run tests"),
                assistant_calls("bash"),
                user("New Terminal Output:\nerror: test command failed"),
                assistant_calls("bash"),
                user("New Terminal Output:\nfirst repair command completed"),
                assistant_calls("bash"),
                user("New Terminal Output:\nsecond repair command completed"),
                assistant_calls("bash"),
                user("New Terminal Output:\nthird repair command completed"),
                assistant_calls("bash"),
            ],
            Vec::new(),
        );

        let ir = extract(&prompt);

        assert_eq!(ir.recovery_signal, RecoverySignal::None);
        assert_eq!(ir.state_kind, WorkflowStateKind::ToolFollowup);
    }

    #[test]
    fn generic_splits_command_not_found_tool_followup_into_recovery_state() {
        let prompt = prompt(
            vec![
                user("verify the regex"),
                assistant_calls("bash"),
                tool_result("/bin/bash: line 1: python3: command not found"),
            ],
            Vec::new(),
        );
        let ir = extract(&prompt);
        assert_eq!(ir.recovery_signal, RecoverySignal::LikelyRecovery);
        assert_eq!(ir.state_kind, WorkflowStateKind::Recovery);
        assert_eq!(ir.last_tool_name.as_deref(), Some("bash"));
    }

    #[test]
    fn generic_does_not_treat_task_or_echoed_source_words_as_recovery() {
        let prompt = prompt(
            vec![
                user("fix the failed certificate verification"),
                assistant_calls("bash"),
                user(
                    "New Terminal Output:\n\
root@box:/app# cat > check.py <<'PY'\n\
> print(\"Certificate verification failed\")\n\
> PY\n\
root@box:/app# python check.py\n\
Certificate verification successful\n\
root@box:/app#",
                ),
            ],
            Vec::new(),
        );

        let ir = extract(&prompt);

        assert_eq!(ir.recovery_signal, RecoverySignal::None);
        assert_eq!(ir.state_kind, WorkflowStateKind::ToolFollowup);
    }

    #[test]
    fn generic_trusts_typed_tool_error_without_keyword_guessing() {
        let prompt = prompt(
            vec![
                user("run the command"),
                assistant_calls("bash"),
                tool_error_result("operation could not complete"),
            ],
            Vec::new(),
        );

        let ir = extract(&prompt);

        assert_eq!(ir.recovery_signal, RecoverySignal::LikelyRecovery);
        assert_eq!(ir.state_kind, WorkflowStateKind::Recovery);
    }

    #[test]
    fn generic_uses_explicit_nonzero_exit_status_from_plain_tool_output() {
        let prompt = prompt(
            vec![
                user("run the command"),
                assistant_calls("bash"),
                tool_result("Process exited with code 2\nFinal output:\ninvalid input"),
            ],
            Vec::new(),
        );

        let ir = extract(&prompt);

        assert_eq!(ir.recovery_signal, RecoverySignal::LikelyRecovery);
        assert_eq!(ir.state_kind, WorkflowStateKind::Recovery);
    }

    #[test]
    fn generic_buckets_context_size() {
        let large = "x".repeat(80_000);
        let prompt = prompt(vec![user(&large)], Vec::new());
        let ir = extract(&prompt);
        assert_eq!(ir.context_size, ContextSizeBucket::Large);
    }

    #[test]
    fn generic_guards_a_long_tool_trajectory_as_high_redo_risk() {
        let mut messages = vec![user("finish the task")];
        for turn in 0..8 {
            messages.push(assistant_calls("bash"));
            messages.push(tool_result(&format!("step {turn} completed")));
        }
        let prompt = prompt(messages, Vec::new());

        let ir = extract(&prompt);

        assert_eq!(
            ir.capability_constraints.expected_redo_penalty,
            RequirementLevel::High
        );
        assert_eq!(
            ir.route_projection().key(),
            "agent_trace/v2|tool_followup|guarded"
        );
        assert!(ir.evidence.iter().any(|evidence| {
            evidence.kind == "trajectory_pressure" && evidence.value == "assistant_turns:8"
        }));
    }

    #[test]
    fn generic_does_not_guard_a_long_conversation_without_agent_actions() {
        let mut messages = vec![user("talk through the design")];
        for turn in 0..8 {
            messages.push(Message::text(Role::Assistant, format!("answer {turn}")));
            messages.push(user(&format!("follow-up {turn}")));
        }
        let prompt = prompt(messages, Vec::new());

        let ir = extract(&prompt);

        assert_eq!(
            ir.capability_constraints.expected_redo_penalty,
            RequirementLevel::Medium
        );
        assert_eq!(
            ir.route_projection().key(),
            "agent_trace/v2|planning|normal"
        );
        assert!(
            ir.evidence
                .iter()
                .all(|evidence| evidence.kind != "trajectory_pressure")
        );
    }

    #[test]
    fn generic_does_not_treat_a_declared_but_unused_tool_as_an_agent_trajectory() {
        let mut messages = vec![user("talk through the design")];
        for turn in 0..8 {
            messages.push(Message::text(Role::Assistant, format!("answer {turn}")));
            messages.push(user(&format!("follow-up {turn}")));
        }
        let declared_tool = Tool::Function {
            name: "search".to_string(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
            strict: None,
            provider_metadata: ProviderMetadata::new(),
        };
        let prompt = prompt(messages, vec![declared_tool]);

        let ir = extract(&prompt);

        assert_eq!(
            ir.capability_constraints.expected_redo_penalty,
            RequirementLevel::Medium
        );
        assert_eq!(
            ir.route_projection().key(),
            "agent_trace/v2|planning|normal"
        );
    }
}
