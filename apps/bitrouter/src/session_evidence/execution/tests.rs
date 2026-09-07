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
