use super::*;
use crate::session_evidence::types::{Harness, RecordInput};
use serde_json::json;

fn facts(format: SourceFormat, raw: Value) -> Result<Vec<NativeFact>> {
    let harness = if matches!(
        format,
        SourceFormat::CodexRollout | SourceFormat::CodexAppServer
    ) {
        Harness::Codex
    } else {
        Harness::ClaudeCode
    };
    let source = SourceDescriptor {
        namespace: "profile".into(),
        harness,
        format,
        locator: "fixture".into(),
        node: Some(NodeKey {
            namespace: "profile".into(),
            harness,
            native_id: "root".into(),
            agent_id: None,
        }),
    };
    let source_id = source.id(&canonical_digest(&"local")?)?;
    let input = RecordInput {
        generation: "fixture/1".into(),
        sequence: 0,
        byte_start: None,
        byte_end: None,
        producer_version: None,
        raw,
    };
    let record = StoredRecord {
        id: input.id(&source_id)?,
        source_id,
        digest: canonical_digest(&input)?,
        input,
    };
    extract(&source, &record)
}

#[test]
fn codex_rollout_preserves_failed_completion_and_event_aliases() -> Result<()> {
    for kind in ["task_complete", "turn_complete"] {
        for error in [Value::Null, json!({"message":"request failed"})] {
            let parsed = facts(
                SourceFormat::CodexRollout,
                json!({"type":"event_msg","payload":{"type":kind,"turn_id":"turn","error":error}}),
            )?;
            assert_eq!(parsed.len(), 1);
            assert_eq!(
                parsed[0].event,
                FactKind::RunFinished {
                    run_id: "turn".into(),
                    status: if error.is_null() {
                        "completed"
                    } else {
                        "failed"
                    }
                    .into()
                }
            );
        }
    }
    for kind in ["task_started", "turn_started"] {
        let parsed = facts(
            SourceFormat::CodexRollout,
            json!({"type":"event_msg","payload":{"type":kind,"turn_id":"turn"}}),
        )?;
        assert_eq!(
            parsed[0].event,
            FactKind::RunStarted {
                run_id: "turn".into()
            }
        );
    }
    Ok(())
}

#[test]
fn codex_rollout_keeps_both_explicit_parent_claims_for_conflict_detection() -> Result<()> {
    let parsed = facts(
        SourceFormat::CodexRollout,
        json!({"type":"session_meta","payload":{
            "id":"root","parent_thread_id":"parent-one","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-two"}}}
        }}),
    )?;
    let parents: BTreeSet<_> = parsed
        .iter()
        .filter(|fact| {
            matches!(
                fact.event,
                FactKind::Relation {
                    relation: EdgeKind::Spawn
                }
            )
        })
        .filter_map(|fact| {
            fact.related_node
                .as_ref()
                .map(|node| node.native_id.as_str())
        })
        .collect();
    assert_eq!(parents, BTreeSet::from(["parent-one", "parent-two"]));
    Ok(())
}

#[test]
fn codex_agent_calls_survive_empty_receivers_and_do_not_end_child_execution() -> Result<()> {
    for (method, status, receivers) in [
        ("item/started", "inProgress", json!([])),
        ("item/completed", "failed", json!([])),
        ("item/completed", "completed", json!(["child"])),
    ] {
        let parsed = facts(
            SourceFormat::CodexAppServer,
            json!({"direction":"server","phase":"notification","method":method,"payload":{
                "threadId":"root","turnId":"turn","item":{"type":"collabAgentToolCall","id":"call","tool":"spawnAgent","status":status,"senderThreadId":"root","receiverThreadIds":receivers}
            }}),
        )?;
        assert!(parsed.iter().any(|fact| matches!(&fact.event, FactKind::AgentCall { call_id, status: parsed_status, .. } if call_id == "call" && parsed_status == status)));
        assert!(
            !parsed
                .iter()
                .any(|fact| matches!(fact.event, FactKind::RunFinished { .. }))
        );
        assert_eq!(
            parsed
                .iter()
                .filter(|fact| matches!(
                    fact.event,
                    FactKind::Relation {
                        relation: EdgeKind::Spawn
                    }
                ))
                .count(),
            usize::from(status == "completed")
        );
    }
    Ok(())
}

#[test]
fn stop_hooks_keep_session_background_scope_and_never_certify_completion() -> Result<()> {
    let parsed = facts(
        SourceFormat::ClaudeHook,
        json!({"method":"SubagentStop","payload":{
            "session_id":"root","agent_id":"child","prompt_id":"prompt",
            "background_tasks":[{"task_id":"different-task"}],"session_crons":[]
        }}),
    )?;
    assert_eq!(parsed.len(), 1);
    assert_eq!(
        parsed[0]
            .node
            .as_ref()
            .and_then(|node| node.agent_id.as_deref()),
        Some("child")
    );
    assert_eq!(
        parsed[0].event,
        FactKind::StopAttempt {
            prompt_id: Some("prompt".into()),
            session_background_work: Some(true)
        }
    );
    let legacy = facts(
        SourceFormat::ClaudeHook,
        json!({"method":"Stop","payload":{"session_id":"root"}}),
    )?;
    assert_eq!(
        legacy[0].event,
        FactKind::StopAttempt {
            prompt_id: None,
            session_background_work: None
        }
    );
    Ok(())
}

#[test]
fn requests_are_not_native_results_and_unknown_terminal_status_is_a_gap() -> Result<()> {
    let mut raw = json!({"direction":"client","phase":"notification","method":"turn/completed","payload":{
        "threadId":"root","turn":{"id":"turn","status":"completed"}
    }});
    assert!(facts(SourceFormat::CodexAppServer, raw.clone())?.is_empty());
    raw["direction"] = json!("server");
    raw["payload"]["turn"]["status"] = json!("future-status");
    let parsed = facts(SourceFormat::CodexAppServer, raw)?;
    assert_eq!(parsed.len(), 1);
    assert!(matches!(parsed[0].event, FactKind::Gap { .. }));
    Ok(())
}

fn sdk_facts(message: Value) -> Result<Vec<NativeFact>> {
    facts(
        SourceFormat::Acp,
        json!({"method":"_claude/sdkMessage","phase":"notification","native_scope":"session",
        "payload":{"sessionId":"root","message":message}}),
    )
}

#[test]
fn claude_sdk_keeps_command_result_idle_and_background_observations_separate() -> Result<()> {
    let command = sdk_facts(
        json!({"type":"command_lifecycle","command_uuid":"command","session_id":"root","state":"completed"}),
    )?;
    assert_eq!(
        command[0].event,
        FactKind::NativeCommand {
            command_id: "command".into(),
            state: "completed".into()
        }
    );
    let result = sdk_facts(
        json!({"type":"result","subtype":"success","uuid":"result","is_error":false,"session_id":"root"}),
    )?;
    assert_eq!(
        result[0].event,
        FactKind::NativeResult {
            result_id: "result".into(),
            command_id: None,
            status: "success".into(),
            is_error: false
        }
    );
    let idle = sdk_facts(
        json!({"type":"system","subtype":"session_state_changed","state":"idle","session_id":"root"}),
    )?;
    assert_eq!(
        idle[0].event,
        FactKind::SessionState {
            state: "idle".into()
        }
    );
    let background = sdk_facts(
        json!({"type":"system","subtype":"background_tasks_changed","session_id":"root","tasks":[{"task_id":"still-running"}]}),
    )?;
    assert_eq!(
        background[0].event,
        FactKind::BackgroundTasks {
            task_ids: BTreeSet::from(["still-running".into()])
        }
    );
    assert!(
        command
            .iter()
            .chain(&result)
            .chain(&idle)
            .chain(&background)
            .all(|fact| !matches!(fact.event, FactKind::RunFinished { .. }))
    );
    Ok(())
}

#[test]
fn claude_sdk_task_ids_do_not_invent_agent_nodes_and_unknown_scope_stays_unbound() -> Result<()> {
    let message = json!({"type":"system","subtype":"task_started","session_id":"root","task_id":"shell-job","tool_use_id":"tool-call","task_type":"local_bash"});
    let task = sdk_facts(message.clone())?;
    assert_eq!(
        task[0]
            .node
            .as_ref()
            .map(|node| (&node.native_id, &node.agent_id)),
        Some((&"root".to_string(), &None))
    );
    assert!(task[0].related_node.is_none());
    assert!(
        matches!(&task[0].event, FactKind::NativeTask { task_id, tool_use_id:Some(tool), .. } if task_id == "shell-job" && tool == "tool-call")
    );
    let unbound = facts(
        SourceFormat::Acp,
        json!({"method":"_claude/sdkMessage","phase":"notification","native_scope":"controller",
        "payload":{"sessionId":"root","message":message}}),
    )?;
    assert!(unbound.is_empty());
    let reset_session = sdk_facts(
        json!({"type":"system","subtype":"session_state_changed","session_id":"other","state":"idle"}),
    )?;
    assert_eq!(reset_session.len(), 1);
    assert!(matches!(
        reset_session[0].event,
        FactKind::SessionState { .. }
    ));
    assert_eq!(
        reset_session[0]
            .node
            .as_ref()
            .map(|node| node.native_id.as_str()),
        Some("other")
    );
    assert_eq!(reset_session[0].acp_session_id.as_deref(), Some("root"));
    let missing_native_id =
        sdk_facts(json!({"type":"system","subtype":"session_state_changed","state":"idle"}))?;
    assert!(matches!(missing_native_id[0].event, FactKind::Gap { .. }));
    let captured = crate::session_evidence::claude_sdk::notification_fields(&json!({"sessionId":"root","message":{
        "type":"system","subtype":"task_started","task_id":"task","tool_use_id":{"prompt":"private"}
    }})).context("selected malformed event")?;
    let malformed = sdk_facts(captured["message"].clone())?;
    assert_eq!(malformed.len(), 1);
    assert!(matches!(malformed[0].event, FactKind::Gap { .. }));
    Ok(())
}
