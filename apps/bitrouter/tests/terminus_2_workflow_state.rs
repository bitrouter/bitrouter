use bitrouter::workflow_state::extractors::{ExtractorInput, extract_workflow_state};
use bitrouter::workflow_state::ir::{
    AgentRole, ContextTransition, HarnessId, ProtocolKind, RequirementLevel, SessionConfidence,
    ToolDensity, WorkflowStateKind,
};
use bitrouter::workflow_state::online::OnlineWorkflowState;
use bitrouter::workflow_state::session::{
    WorkflowIdentityTracker, resolve_session_signal, resolve_workflow_identity,
};
use bitrouter_sdk::HeaderMap;
use bitrouter_sdk::language_model::types::{GenerationParams, Message, Prompt, Role};

const TERMINUS_OPENING: &str = "You are an AI assistant tasked with solving command-line tasks in a Linux environment. Format your response as JSON with analysis, plan, commands, and task_complete.";

fn prompt(messages: Vec<Message>) -> Prompt {
    Prompt {
        model: "inbound".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages,
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    }
}

fn user(text: &str) -> Message {
    Message::text(Role::User, text)
}

fn assistant(text: &str) -> Message {
    Message::text(Role::Assistant, text)
}

fn json_action(commands: &[&str], task_complete: bool) -> String {
    serde_json::json!({
        "analysis": "current state",
        "plan": "next command",
        "commands": commands
            .iter()
            .map(|keystrokes| serde_json::json!({
                "keystrokes": keystrokes,
                "duration": 0.1
            }))
            .collect::<Vec<_>>(),
        "task_complete": task_complete
    })
    .to_string()
}

fn extract(prompt: &Prompt) -> bitrouter::workflow_state::ir::WorkflowStateIR {
    let headers = HeaderMap::new();
    let raw_body = serde_json::json!({});
    extract_workflow_state(&ExtractorInput {
        harness_hint: Some(HarnessId::Terminus2),
        protocol_hint: ProtocolKind::ChatCompletions,
        headers: &headers,
        raw_body: &raw_body,
        prompt,
    })
}

#[test]
fn terminus_harness_aliases_are_stable() {
    for alias in ["terminus_2", "terminus-2", "terminus2"] {
        let harness: HarnessId = serde_json::from_value(serde_json::json!(alias)).unwrap();
        assert_eq!(harness, HarnessId::Terminus2);
    }
    assert_eq!(
        serde_json::to_value(HarnessId::Terminus2).unwrap(),
        serde_json::json!("terminus_2")
    );
}

#[test]
fn official_prompt_contract_detects_terminus_and_defaults_to_chat_completions() {
    let prompt = prompt(vec![user(TERMINUS_OPENING)]);

    let online = OnlineWorkflowState::from_headers(&HeaderMap::new(), &prompt);

    assert_eq!(online.ir.harness_id, HarnessId::Terminus2);
    assert_eq!(online.ir.protocol, ProtocolKind::ChatCompletions);
}

#[test]
fn explicit_protocol_combines_with_terminus_prompt_detection() {
    let prompt = prompt(vec![user(TERMINUS_OPENING)]);
    let mut headers = HeaderMap::new();
    headers.insert("x-bitrouter-protocol", "responses".parse().unwrap());

    let online = OnlineWorkflowState::from_headers(&headers, &prompt);

    assert_eq!(online.ir.harness_id, HarnessId::Terminus2);
    assert_eq!(online.ir.protocol, ProtocolKind::Responses);
}

#[test]
fn explicit_terminus_harness_defaults_to_chat_completions() {
    let prompt = prompt(vec![user("ordinary task text")]);
    let mut headers = HeaderMap::new();
    headers.insert("x-bitrouter-harness", "terminus_2".parse().unwrap());

    let online = OnlineWorkflowState::from_headers(&headers, &prompt);

    assert_eq!(online.ir.harness_id, HarnessId::Terminus2);
    assert_eq!(online.ir.protocol, ProtocolKind::ChatCompletions);
}

#[test]
fn terminus_state_precedence_covers_json_actions() {
    let cases = [
        (vec![], true, "ok", WorkflowStateKind::Finalization),
        (
            vec!["cargo test --all-features\n"],
            false,
            "3 passed",
            WorkflowStateKind::Test,
        ),
        (
            vec!["apply_patch <<'PATCH'\nPATCH\n"],
            false,
            "Done",
            WorkflowStateKind::Edit,
        ),
        (
            vec!["git diff --check\n"],
            false,
            "clean",
            WorkflowStateKind::Review,
        ),
        (
            vec!["make build\n"],
            false,
            "build complete",
            WorkflowStateKind::ToolFollowup,
        ),
        (
            vec!["cargo test\n"],
            false,
            "error: failed",
            WorkflowStateKind::Recovery,
        ),
    ];

    for (commands, complete, result, expected) in cases {
        let prompt = prompt(vec![
            user(TERMINUS_OPENING),
            assistant(&json_action(&commands, complete)),
            user(result),
        ]);
        let ir = extract(&prompt);
        assert_eq!(ir.state_kind, expected);
        if !commands.is_empty() {
            assert_eq!(ir.last_tool_name.as_deref(), Some("tmux_shell"));
            assert_eq!(
                ir.capability_constraints.tool_reliability,
                RequirementLevel::High
            );
            assert!(
                ir.capability_constraints
                    .compatibility
                    .contains(&"requires_terminal_interaction".to_string())
            );
        }
    }
}

#[test]
fn terminus_xml_action_and_command_density_are_recognized() {
    let prompt = prompt(vec![
        user(TERMINUS_OPENING),
        assistant(concat!(
            "<response><analysis>verify</analysis><plan>run tests</plan><commands>",
            "<command><keystrokes duration=\"0.1\">cd /workspace\n</keystrokes></command>",
            "<command><keystrokes duration=\"0.1\">pytest -q\n</keystrokes></command>",
            "</commands><task_complete>false</task_complete></response>"
        )),
        user("3 passed"),
    ]);

    let ir = extract(&prompt);

    assert_eq!(ir.state_kind, WorkflowStateKind::Test);
    assert_eq!(ir.tool_density, ToolDensity::High);
    assert!(ir.evidence.iter().any(|evidence| {
        evidence.kind == "terminus_2_action_format" && evidence.value == "xml"
    }));
}

#[test]
fn oversized_or_malformed_action_fails_open_to_planning() {
    let oversized = format!(
        "{{\"commands\":[{{\"keystrokes\":\"{}\"}}]}}",
        "x".repeat(300_000)
    );
    for action in [oversized, "not an action".to_string()] {
        let prompt = prompt(vec![
            user(TERMINUS_OPENING),
            assistant(&action),
            user("continue"),
        ]);
        assert_eq!(extract(&prompt).state_kind, WorkflowStateKind::Planning);
    }
}

#[test]
fn terminus_session_precedence_is_workflow_then_header_then_body() {
    let prompt = prompt(vec![user(TERMINUS_OPENING)]);
    let raw_body = serde_json::json!({"session_id": "body-session"});
    let mut headers = HeaderMap::new();
    headers.insert("x-session-id", "header-session".parse().unwrap());
    headers.insert(
        "x-bitrouter-workflow-session",
        "workflow-session".parse().unwrap(),
    );

    let resolved = resolve_session_signal(&ExtractorInput {
        harness_hint: Some(HarnessId::Terminus2),
        protocol_hint: ProtocolKind::ChatCompletions,
        headers: &headers,
        raw_body: &raw_body,
        prompt: &prompt,
    });
    assert_eq!(resolved.signal.key.as_deref(), Some("workflow-session"));

    headers.remove("x-bitrouter-workflow-session");
    let resolved = resolve_session_signal(&ExtractorInput {
        harness_hint: Some(HarnessId::Terminus2),
        protocol_hint: ProtocolKind::ChatCompletions,
        headers: &headers,
        raw_body: &raw_body,
        prompt: &prompt,
    });
    assert_eq!(resolved.signal.key.as_deref(), Some("header-session"));
    assert_eq!(resolved.signal.confidence, SessionConfidence::High);

    headers.remove("x-session-id");
    let resolved = resolve_session_signal(&ExtractorInput {
        harness_hint: Some(HarnessId::Terminus2),
        protocol_hint: ProtocolKind::ChatCompletions,
        headers: &headers,
        raw_body: &raw_body,
        prompt: &prompt,
    });
    assert_eq!(resolved.signal.key.as_deref(), Some("body-session"));
}

fn identity_for(
    tracker: &WorkflowIdentityTracker,
    prompt: &Prompt,
    trial_id: &str,
) -> bitrouter::workflow_state::ir::WorkflowIdentity {
    let mut headers = HeaderMap::new();
    headers.insert("x-session-id", "terminus-parent".parse().unwrap());
    headers.insert(
        "x-bitrouter-benchmark-run-id",
        "short13-run".parse().unwrap(),
    );
    headers.insert("x-bitrouter-trial-id", trial_id.parse().unwrap());
    let raw_body = serde_json::json!({});
    resolve_workflow_identity(
        &ExtractorInput {
            harness_hint: Some(HarnessId::Terminus2),
            protocol_hint: ProtocolKind::ChatCompletions,
            headers: &headers,
            raw_body: &raw_body,
            prompt,
        },
        tracker,
    )
}

fn identity_for_session(
    tracker: &WorkflowIdentityTracker,
    session_id: &str,
    prompt: &Prompt,
) -> bitrouter::workflow_state::ir::WorkflowIdentity {
    let mut headers = HeaderMap::new();
    headers.insert("x-session-id", session_id.parse().unwrap());
    headers.insert(
        "x-bitrouter-benchmark-run-id",
        "short13-run".parse().unwrap(),
    );
    headers.insert("x-bitrouter-trial-id", "trial-a".parse().unwrap());
    let raw_body = serde_json::json!({});
    resolve_workflow_identity(
        &ExtractorInput {
            harness_hint: Some(HarnessId::Terminus2),
            protocol_hint: ProtocolKind::ChatCompletions,
            headers: &headers,
            raw_body: &raw_body,
            prompt,
        },
        tracker,
    )
}

#[test]
fn official_terminus_session_suffixes_join_subagents_to_parent_epoch() {
    let tracker = WorkflowIdentityTracker::default();
    let root = "123e4567-e89b-12d3-a456-426614174000";
    let answers_session = format!("{root}-summarization-1-answers");
    let main_0 = identity_for_session(&tracker, root, &prompt(vec![user(TERMINUS_OPENING)]));
    let summary = identity_for_session(
        &tracker,
        &format!("{root}-summarization-1-summary"),
        &prompt(vec![user(
            "You are about to hand off your work to another AI agent.",
        )]),
    );
    let questions = identity_for_session(
        &tracker,
        &format!("{root}-summarization-1-questions"),
        &prompt(vec![user(
            "You are picking up work from a previous AI agent on this task:",
        )]),
    );
    let answers = identity_for_session(
        &tracker,
        &answers_session,
        &prompt(vec![user(
            "The next agent has a few questions for you, please answer each of them one by one in detail.",
        )]),
    );
    let main_1 = identity_for_session(
        &tracker,
        &format!("{root}-cont-1"),
        &prompt(vec![user(
            "Here are the answers the other agent provided. Continue working on this task.",
        )]),
    );

    assert_eq!(main_0.parent_session_id.as_deref(), Some(root));
    assert_eq!(main_0.role, AgentRole::Main);
    assert_eq!(main_0.context_epoch, 0);
    assert_eq!(summary.parent_session_id.as_deref(), Some(root));
    assert_eq!(questions.parent_session_id.as_deref(), Some(root));
    assert_eq!(answers.parent_session_id.as_deref(), Some(root));
    assert_eq!(main_1.parent_session_id.as_deref(), Some(root));
    assert_eq!(summary.role, AgentRole::Summary);
    assert_eq!(questions.role, AgentRole::Questions);
    assert_eq!(answers.role, AgentRole::Answers);
    assert_eq!(main_1.role, AgentRole::Main);
    assert_eq!(summary.context_epoch, 1);
    assert_eq!(questions.context_epoch, 1);
    assert_eq!(answers.context_epoch, 1);
    assert_eq!(main_1.context_epoch, 1);
    assert_eq!(summary.transition, ContextTransition::CompactionStart);
    assert_eq!(
        questions.transition,
        ContextTransition::CompactionContinuation
    );
    assert_eq!(
        answers.transition,
        ContextTransition::CompactionContinuation
    );
    assert_eq!(main_1.transition, ContextTransition::MainResume);
    assert_eq!(summary.fingerprint, questions.fingerprint);
    assert_eq!(questions.fingerprint, answers.fingerprint);
    assert_eq!(answers.fingerprint, main_1.fingerprint);
    assert_eq!(
        answers.agent_session_id.as_deref(),
        Some(answers_session.as_str())
    );
}

#[test]
fn terminus_compaction_roles_share_one_incremented_context_epoch() {
    let tracker = WorkflowIdentityTracker::default();
    let main_0 = identity_for(&tracker, &prompt(vec![user(TERMINUS_OPENING)]), "trial-a");
    let summary = identity_for(
        &tracker,
        &prompt(vec![user(
            "You are about to hand off your work to another AI agent.",
        )]),
        "trial-a",
    );
    let questions = identity_for(
        &tracker,
        &prompt(vec![user(
            "You are picking up work from a previous AI agent on this task:",
        )]),
        "trial-a",
    );
    let answers = identity_for(
        &tracker,
        &prompt(vec![user(
            "The next agent has a few questions for you, please answer each of them one by one in detail.",
        )]),
        "trial-a",
    );
    let main_1 = identity_for(
        &tracker,
        &prompt(vec![user(
            "Here are the answers the other agent provided. Continue the task.",
        )]),
        "trial-a",
    );

    assert_eq!(main_0.role, AgentRole::Main);
    assert_eq!(main_0.context_epoch, 0);
    assert_eq!(summary.role, AgentRole::Summary);
    assert_eq!(summary.context_epoch, 1);
    assert_eq!(summary.transition, ContextTransition::CompactionStart);
    assert_eq!(questions.role, AgentRole::Questions);
    assert_eq!(answers.role, AgentRole::Answers);
    assert_eq!(questions.context_epoch, 1);
    assert_eq!(answers.context_epoch, 1);
    assert_eq!(main_1.context_epoch, 1);
    assert_eq!(main_1.transition, ContextTransition::MainResume);
    assert_eq!(summary.fingerprint, questions.fingerprint);
    assert_eq!(questions.fingerprint, answers.fingerprint);
    assert_eq!(answers.fingerprint, main_1.fingerprint);
    assert_ne!(main_0.fingerprint, main_1.fingerprint);
}

#[test]
fn terminus_context_epochs_are_isolated_by_trial() {
    let tracker = WorkflowIdentityTracker::default();
    let summary_a = identity_for(
        &tracker,
        &prompt(vec![user(
            "You are about to hand off your work to another AI agent.",
        )]),
        "trial-a",
    );
    let main_b = identity_for(&tracker, &prompt(vec![user(TERMINUS_OPENING)]), "trial-b");

    assert_eq!(summary_a.context_epoch, 1);
    assert_eq!(main_b.context_epoch, 0);
    assert_ne!(summary_a.fingerprint, main_b.fingerprint);
}

#[test]
fn explicit_identity_headers_win_over_inference() {
    let tracker = WorkflowIdentityTracker::default();
    let prompt = prompt(vec![user(TERMINUS_OPENING)]);
    let mut headers = HeaderMap::new();
    headers.insert("x-session-id", "fallback".parse().unwrap());
    headers.insert(
        "x-bitrouter-parent-session-id",
        "explicit-parent".parse().unwrap(),
    );
    headers.insert(
        "x-bitrouter-agent-session-id",
        "explicit-agent".parse().unwrap(),
    );
    headers.insert("x-bitrouter-agent-role", "answers".parse().unwrap());
    headers.insert("x-bitrouter-context-epoch", "7".parse().unwrap());
    headers.insert(
        "x-bitrouter-session-fingerprint",
        "explicit-fingerprint".parse().unwrap(),
    );
    let raw_body = serde_json::json!({});

    let identity = resolve_workflow_identity(
        &ExtractorInput {
            harness_hint: Some(HarnessId::Terminus2),
            protocol_hint: ProtocolKind::ChatCompletions,
            headers: &headers,
            raw_body: &raw_body,
            prompt: &prompt,
        },
        &tracker,
    );

    assert_eq!(
        identity.parent_session_id.as_deref(),
        Some("explicit-parent")
    );
    assert_eq!(identity.agent_session_id.as_deref(), Some("explicit-agent"));
    assert_eq!(identity.role, AgentRole::Answers);
    assert_eq!(identity.context_epoch, 7);
    assert_eq!(identity.fingerprint, "explicit-fingerprint");
}
