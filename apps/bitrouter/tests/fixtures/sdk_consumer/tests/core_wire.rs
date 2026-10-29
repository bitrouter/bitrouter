//! Every public channel variant has a literal wire fixture outside the workspace.
use anyhow::{Result, ensure};
use bitrouter_ai::core::protocol::{ClientMessage, Command, ServerMessage};
use serde_json::{Value, json};

fn head() -> Value {
    json!({"execution_epoch":1,"state_revision":2,"event_seq":3,"batch_id":"batch","payload_sha256":"digest"})
}
fn batch() -> Value {
    json!({"identity":{"batch_id":"batch","session_id":"session","execution_epoch":1,"core_instance_id":"core"},
    "payload_encoding":"base64-json","payload_bytes":"opaque original bytes=\n","payload_sha256":"unverified digest"})
}
fn manifest() -> Value {
    json!({"tools":[],"tool_manifest_digest":"digest","workspace_id":"workspace","workspace_revision":null,
    "permission_revision":1,"max_tool_output_bytes":1024,"artifact_quota_bytes":2048,"max_artifact_chunk_bytes":512,"required_features":[]})
}
fn limits() -> Value {
    json!({"active_models":1,"agents":2,"child_depth":3,"model_attempts":4,"outstanding_tools":5,"total_tools":6,
    "queued_runs":7,"mailbox_messages":8,"active_seconds":9,"checkpoint_bytes":1024,"unacknowledged_bytes":2048,"ephemeral_bytes":4096,"input_bytes":512})
}
fn binding() -> Value {
    json!({"grant":{"session_id":"session","harness_id":"harness","core_instance_id":"core","execution_epoch":1},
    "durable_head":head(),"checkpoint":batch(),"manifest":manifest(),"limits":limits()})
}
fn artifact() -> Value {
    json!({"artifact_id":"artifact","sha256":"digest","bytes":3,"media_type":"text/plain"})
}
fn tool_result() -> Value {
    json!({"invocation_id":"invocation","attempt_id":"attempt","status":"succeeded","output":"mock result","evidence":[artifact()],"workspace_revision":null})
}
fn report() -> Value {
    json!({"request_id":"request","attempt_index":0,"route":{"provider":"provider","model":"model","protocol":"responses",
    "constraints":{"capabilities":[],"token_limits":{"max_input_tokens":null,"max_output_tokens":128,"context_window":null},"input_token_counting":null,"source":null},
    "output_token_limit_supported":true,"input_count":null,"protocol_validation":{"status":"compatible"},"continuation":{"status":"unknown"}},
    "actual_provider":null,"actual_model":null,"result":null,"error":"fixture failure","elapsed_ms":0,
    "token_cost":{"status":"unknown","reason":"fixture"},"cache":{"read_tokens":null,"write_tokens":null,"source":"unknown","unknown_reason":"fixture"},
    "private_context":{"input":{"status":"unknown"},"output":{"status":"unknown"}},"continuation":{"input":{"status":"unknown"},"output":{"status":"unknown"}}})
}
fn error() -> Value {
    json!({"code":"stale_epoch","message":"fixture rejection","commit_status":"not_committed"})
}

fn client_tag(command: &Command) -> &'static str {
    match command {
        Command::Bind(_) => "session.bind",
        Command::Restore(_) => "session.restore",
        Command::Enqueue(_) => "input.enqueue",
        Command::Steer { .. } => "input.steer",
        Command::CancelRun { .. } => "run.cancel",
        Command::CancelAgent { .. } => "agent.cancel",
        Command::ResumeQueue => "queue.resume",
        Command::Signals(_) => "signals.update",
        Command::ToolResult(_) => "tool.result",
        Command::ToolStatus(_) => "tool.status",
        Command::ProviderEvidence(_) => "model.evidence",
        Command::Material { .. } => "material.result",
        Command::Ack(_) => "checkpoint.ack",
        Command::ArtifactChunk { .. } => "artifact.chunk",
        Command::Head { .. } => "session.head",
        Command::Operation { .. } => "operation.get",
        Command::Release => "session.release",
    }
}
fn server_tag(message: &ServerMessage) -> &'static str {
    match message {
        ServerMessage::Checkpoint(_) => "checkpoint.proposed",
        ServerMessage::ToolExecute(_) => "tool.execute",
        ServerMessage::ToolCancel { .. } => "tool.cancel",
        ServerMessage::MaterialRequest { .. } => "material.request",
        ServerMessage::SkillConsentRequest(_) => "skill.consent.request",
        ServerMessage::ArtifactPut { .. } => "artifact.put",
        ServerMessage::ArtifactRead { .. } => "artifact.read",
        ServerMessage::Receipt(_) => "operation.receipt",
        ServerMessage::UnknownOperation { .. } => "operation.unknown",
        ServerMessage::Head(_) => "session.head",
        ServerMessage::Error(_) => "error",
    }
}

#[test]
fn all_client_variants_preserve_literal_wire_contracts() -> Result<()> {
    let cases = vec![
        ("session.bind", Some(binding())),
        (
            "session.restore",
            Some(
                json!({"binding":binding(),"journal_tail":[batch()],"tools":[],"results":[tool_result()],"available_artifacts":[artifact()],"previous_owner_stopped":true,"active_time":null}),
            ),
        ),
        (
            "input.enqueue",
            Some(
                json!({"text":"task","model":"model","effort":null,"max_output_tokens":128,"routing":{"model":"fixed","context":"auto"},
            "discardable_history":null,"acceptance_criteria":[],"required_materials":[],"verification":null,"limits":null}),
            ),
        ),
        (
            "input.steer",
            Some(json!({"run_id":"run","agent_turn_id":"turn","text":"steer"})),
        ),
        ("run.cancel", Some(json!({"run_id":"run"}))),
        ("agent.cancel", Some(json!({"agent_id":"agent"}))),
        ("queue.resume", None),
        (
            "signals.update",
            Some(
                json!({"signal_revision":1,"observed_at":"2026-10-17T00:00:00Z","scope":"workspace","source":"fixture",
            "workspace_revision":null,"manifest":manifest(),"materials":[],"facts":{"ready":true}}),
            ),
        ),
        ("tool.result", Some(tool_result())),
        (
            "tool.status",
            Some(
                json!({"invocation_id":"invocation","attempt_id":"attempt","status":"waiting_approval","evidence":[]}),
            ),
        ),
        (
            "model.evidence",
            Some(json!({"run_id":"run","attempt_id":"attempt","report":report(),"active_ms":null})),
        ),
        (
            "material.result",
            Some(
                json!({"request_id":"request","material":null,"unavailable_reason":"fixture unavailable"}),
            ),
        ),
        (
            "checkpoint.ack",
            Some(
                json!({"batch_id":"batch","payload_sha256":"digest","through_event_seq":3,"state_revision":2,"execution_epoch":1}),
            ),
        ),
        (
            "artifact.chunk",
            Some(
                json!({"request_id":"request","reference":artifact(),"offset":0,"content_base64":"YWJj"}),
            ),
        ),
        ("session.head", Some(json!({"durable_head":head()}))),
        (
            "operation.get",
            Some(json!({"target_operation_id":"prior"})),
        ),
        ("session.release", None),
    ];
    ensure!(cases.len() == 17);
    let mut seen = std::collections::BTreeSet::new();
    for (tag, payload) in cases {
        for revision in [None, Some(2)] {
            let mut wire = json!({"version":1,"session_id":"session","execution_epoch":1,"operation_id":"operation","type":tag});
            if let Some(payload) = &payload {
                wire["payload"] = payload.clone();
            }
            if let Some(revision) = revision {
                wire["expected_state_revision"] = json!(revision);
            }
            let decoded: ClientMessage = serde_json::from_value(wire.clone())?;
            ensure!(
                client_tag(&decoded.command) == tag && decoded.expected_state_revision == revision
            );
            ensure!(
                serde_json::to_value(&decoded)? == wire,
                "client wire drift: {tag}"
            );
            seen.insert(client_tag(&decoded.command));
        }
    }
    ensure!(seen.len() == 17);
    Ok(())
}

#[test]
fn all_server_variants_preserve_literal_wire_contracts() -> Result<()> {
    let cases = vec![
        ("checkpoint.proposed", batch()),
        (
            "tool.execute",
            json!({"invocation_id":"invocation","attempt_id":"attempt","run_id":"run","agent_id":"agent","agent_turn_id":"turn","step_id":"step",
            "context_revision":1,"tool":"read","arguments":{"path":"file.txt"},"tool_manifest_digest":"digest","permission_revision":1,"workspace_id":"workspace",
            "execution_epoch":1,"authorizing_event_seq":3,"verification":false,"result_limits":{"output_bytes":1024,"payload_bytes":2048,"artifact_bytes":0},"response_id":"response"}),
        ),
        (
            "tool.cancel",
            json!({"invocation_id":"invocation","attempt_id":"attempt","execution_epoch":1}),
        ),
        (
            "material.request",
            json!({"request_id":"request","material_id":"material","version":"v1"}),
        ),
        (
            "skill.consent.request",
            json!({"session_id":"session","run_id":"run","agent_id":"agent","agent_turn_id":"turn",
            "consent":{"request_id":"consent","execution_epoch":1,"permission_revision":1,"tool_manifest_digest":"tools",
            "skill_id":"source","entry_digest":"entry","approved":null,"answer_operation_id":null,"revoked":false},
            "source":{"origin":{"server":"fixture","configuration_digest":"configuration",
            "scope":{"workspace":"workspace","api_key_id":"key","user_id":"user","local":false}},
            "entry":{"uri":"skill://review/SKILL.md","frontmatter":{"name":"review","description":"fixture"},
            "resources":[{"uri":"skill://review/SKILL.md","digest":"sha256:fixture","size":3}]},"entry_digest":"entry"}}),
        ),
        (
            "artifact.put",
            json!({"reference":artifact(),"offset":0,"content_base64":"YWJj"}),
        ),
        (
            "artifact.read",
            json!({"request_id":"request","reference":artifact(),"offset":0,"max_bytes":3}),
        ),
        (
            "operation.receipt",
            json!({"operation_id":"operation","request_sha256":"digest","disposition":"rejected","assigned_ids":{},"state_revision":2,"error":error()}),
        ),
        ("operation.unknown", json!({"operation_id":"missing"})),
        ("session.head", head()),
        ("error", error()),
    ];
    ensure!(cases.len() == 11);
    let mut seen = std::collections::BTreeSet::new();
    for (tag, payload) in cases {
        let wire = json!({"type":tag,"payload":payload});
        let decoded: ServerMessage = serde_json::from_value(wire.clone())?;
        ensure!(server_tag(&decoded) == tag);
        ensure!(
            serde_json::to_value(&decoded)? == wire,
            "server wire drift: {tag}"
        );
        seen.insert(server_tag(&decoded));
    }
    ensure!(seen.len() == 11);
    Ok(())
}
