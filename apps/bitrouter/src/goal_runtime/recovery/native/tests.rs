use super::*;
use bitrouter_ai::types::{Message, ToolResultOutput};
use bitrouter_orchestrator::turn::VerificationStatus;

fn history(name: &str, arguments: &str) -> Vec<ThreadChange> {
    let call = CallRecord {
        origin: CallOrigin::Model,
        item_id: "stable-tool".into(),
        provider_call_id: "provider-call".into(),
        name: name.into(),
        arguments: arguments.into(),
    };
    vec![
        ThreadChange::AssistantResponse {
            turn_id: "turn".into(),
            step_id: "step".into(),
            item_id: "response".into(),
            request_id: "request".into(),
            requested_model: "selector".into(),
            usage: None,
            message: Message::text(Role::Assistant, "candidate call"),
            calls: vec![call.clone()],
        },
        ThreadChange::ToolIntent {
            turn_id: "turn".into(),
            step_id: "step".into(),
            call,
        },
        ThreadChange::ToolResult {
            turn_id: "turn".into(),
            step_id: "step".into(),
            item_id: "stable-tool".into(),
            message: Message {
                role: Role::Tool,
                content: vec![Content::ToolResult {
                    call_id: "provider-call".into(),
                    tool_name: Some(name.into()),
                    dynamic: false,
                    output: ToolResultOutput::Text {
                        value: "actual result".into(),
                    },
                    provider_metadata: Default::default(),
                }],
            },
            effect: EffectStatus::Completed,
        },
        ThreadChange::TurnLifecycle {
            turn_id: "turn".into(),
            lifecycle: TurnLifecycle::Finished {
                status: TurnStatus::Completed,
                detail: "settled".into(),
                final_answer: Some("candidate".into()),
                verification: VerificationStatus::NotRequested,
                verification_evidence: None,
                unknown_effect: false,
            },
        },
    ]
}

async fn verify(changes: &[ThreadChange], workspace: &Path) -> Result<()> {
    let mut effects = Effects::default();
    for change in changes {
        effects.observe("turn", change)?;
    }
    effects.verify(TurnStatus::Completed, workspace).await
}

#[tokio::test]
async fn native_replacement_requires_matching_settled_effects_and_terminal_history() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    // A raw model argument and its canonical native dispatch may differ in
    // whitespace while retaining exactly the same operation and scope.
    let mut original = history(
        "write",
        "{ \"path\": \"result.txt\", \"content\": \"value\" }",
    );
    if let ThreadChange::ToolIntent { call, .. } = &mut original[1] {
        call.arguments = serde_json::json!({"content":"value","path":"result.txt"}).to_string();
    }
    verify(&original, workspace.path()).await?;
    for case in [
        "unknown",
        "no-intent",
        "no-result",
        "no-terminal",
        "wrong-step",
        "wrong-call",
        "changed-args",
        "repeated-result",
        "late-result",
    ] {
        let mut changes = original.clone();
        match case {
            "unknown" => {
                if let ThreadChange::ToolResult { effect, .. } = &mut changes[2] {
                    *effect = EffectStatus::Unknown;
                }
            }
            "no-intent" => {
                changes.remove(1);
            }
            "no-result" => {
                changes.remove(2);
            }
            "no-terminal" => {
                changes.pop();
            }
            "wrong-step" => {
                if let ThreadChange::ToolResult { step_id, .. } = &mut changes[2] {
                    *step_id = "another-step".into();
                }
            }
            "wrong-call" => {
                if let ThreadChange::ToolResult { item_id, .. } = &mut changes[2] {
                    *item_id = "another-call".into();
                }
            }
            "changed-args" => {
                if let ThreadChange::ToolIntent { call, .. } = &mut changes[1] {
                    call.arguments =
                        serde_json::json!({"path":"another.txt","content":"value"}).to_string();
                }
            }
            "repeated-result" => {
                changes.insert(3, changes[2].clone());
            }
            "late-result" => {
                changes.swap(2, 3);
            }
            _ => anyhow::bail!("unrecognized fault"),
        }
        assert!(verify(&changes, workspace.path()).await.is_err(), "{case}");
    }
    Ok(())
}

#[tokio::test]
async fn native_effect_scope_distinguishes_known_local_search_from_external_or_unscoped_work()
-> Result<()> {
    let workspace = tempfile::tempdir()?;
    for (name, arguments, allowed) in [
        ("read", r#"{"path":"result.txt"}"#, true),
        ("glob", r#"{"pattern":"**/*.rs"}"#, true),
        ("grep", r#"{"pattern":"needle","path":null}"#, true),
        ("edit", r#"{"path":"../outside.txt"}"#, false),
        ("edit", r#"{"path":".git/config"}"#, false),
        ("edit", r#"{"path":".GiT/config"}"#, false),
        ("shell", r#"{"command":"true"}"#, false),
        ("mcp__external__write", r#"{"path":"local.txt"}"#, false),
        ("write", r#"{"content":"no path"}"#, false),
    ] {
        assert_eq!(
            verify(&history(name, arguments), workspace.path())
                .await
                .is_ok(),
            allowed,
            "{name}: {arguments}"
        );
    }
    Ok(())
}
