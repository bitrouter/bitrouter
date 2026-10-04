use std::path::PathBuf;

use super::*;
use crate::agent::AgentConfig;
use crate::thread::{PermissionProfile, ThreadStatus};

#[test]
fn failed_startup_reconstruction_stops_growing_context_but_tracks_later_owner_epoch()
-> Result<(), Box<dyn std::error::Error>> {
    let limits = RuntimeLimits {
        context_bytes_per_thread: 4096,
        ..RuntimeLimits::default()
    };
    let snapshot = crate::thread::ThreadSnapshot {
        server_instance_id: "first-writer".into(),
        thread_id: "thread".into(),
        status: ThreadStatus::Idle,
        workspace: PathBuf::from("/workspace"),
        model: "model".into(),
        permission_profile: PermissionProfile::ReadOnly,
        context_version: 0,
        cursor: 0,
        active_turn_id: None,
        queued: Vec::new(),
        pause_reason: None,
        waiting_for_capacity: false,
    };
    let header = ExecutionRecord::ThreadCreated {
        caller: CallerContext::local(),
        snapshot,
        config: Box::new(AgentConfig::fixed("model", None).read_only()),
        verification_command: None,
    };
    let mut audit = StartupAudit::new(&header, "thread", limits.clone())?;
    audit.consume(vec![
        header,
        ExecutionRecord::TurnQueued {
            turn_id: "turn".into(),
            user_item_id: "user".into(),
            prompt: "input".into(),
            queue_order: 1,
        },
        ExecutionRecord::TurnActivated {
            turn_id: "turn".into(),
            context_version: 0,
        },
    ])?;
    let mut records = Vec::new();
    for index in 0..1000 {
        records.push(ExecutionRecord::TurnRecord {
            turn_id: "turn".into(),
            fact: Box::new(ExecutionRecord::ModelRequest {
                step_id: format!("step-{index}"),
                item_id: format!("assistant-{index}"),
                context_version: 0,
                prompt: Box::new(bitrouter_sdk::language_model::Prompt {
                    model: "model".into(),
                    system: None,
                    system_provider_metadata: Default::default(),
                    messages: vec![Message::text(Role::User, "input")],
                    tools: Vec::new(),
                    params: Default::default(),
                    response_format: None,
                    tool_choice: None,
                    stream: true,
                }),
            }),
        });
    }
    records.push(ExecutionRecord::ThreadEvent {
        event: crate::thread::ThreadEvent {
            server_instance_id: "later-writer".into(),
            thread_id: "thread".into(),
            seq: 1004,
            timestamp_ms: 0,
            changes: Vec::new(),
        },
    });
    audit.consume(records)?;
    assert!(!audit.valid);
    let active = audit
        .rebuild
        .active
        .as_ref()
        .ok_or("active recovery state missing")?;
    assert!(serde_json::to_vec(active)?.len() < limits.context_bytes_per_thread * 3);
    assert_eq!(audit.epoch(), "later-writer");
    assert!(!audit.known_clean(Some(&crate::store::ExecutionOwner {
        server_instance_id: "later-writer".into(),
        generation: 2,
        stopped_at_ms: Some(1),
    })));
    Ok(())
}
