//! Original permission requests and separately declared, earlier tool context.
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::supervisor::{PendingPermissionSnapshot, ReplayBatch, SessionEvent, SessionEventKind};

const CONTEXT_FIELD: &str = "earlier_tool_context";
const MAX_CONTEXT_BYTES: usize = 64 * 1024;
const MAX_DECLARATIONS: usize = 32;
mod failures;
pub(crate) mod native;

pub(crate) fn decision_value(
    bytes: &[u8],
    decision: &bitrouter_orchestrator::goal::decision::PermissionDecision,
) -> Result<Value> {
    if let Some(check) = &decision.check {
        let material: bitrouter_orchestrator::goal::verification::permission::CheckPermission =
            serde_json::from_slice(bytes)?;
        anyhow::ensure!(
            &material == check.as_ref(),
            "check permission material changed"
        );
        return Ok(serde_json::json!({
            "permission_id":decision.permission_id,
            "command":check.specification.command(),
            "scope":check,
            "warning":"Repository-controlled commands can execute arbitrary code with the host's privileges. This permission applies only to this check and artifact."
        }));
    }
    match decision.run_id.as_deref() {
        Some(run_id) => {
            let material = checked_value(bytes, Some(run_id))?;
            anyhow::ensure!(
                material["permission_id"].as_str() == Some(decision.permission_id.as_str()),
                "permission material has a different request identity"
            );
            Ok(material)
        }
        None => {
            let request = native::Request::decode(bytes)?;
            anyhow::ensure!(
                request.input.request_id == decision.permission_id,
                "native permission material has a different request identity"
            );
            serde_json::to_value(request).context("serialize native permission material")
        }
    }
}

pub(crate) fn decision_tool(
    bytes: &[u8],
    decision: &bitrouter_orchestrator::goal::decision::PermissionDecision,
) -> Result<Option<Value>> {
    let material = decision_value(bytes, decision)?;
    if decision.check.is_some() {
        return Ok(Some(
            serde_json::json!({"kind":"execute","title":material["command"],"rawInput":material}),
        ));
    }
    match decision.run_id.as_deref() {
        Some(_) => effective_tool_from_checked(&material),
        None => Ok(native::tool_scope(
            material["input"]["tool_name"]
                .as_str()
                .context("native tool name missing")?,
            material["input"]["arguments"]
                .as_str()
                .context("native arguments missing")?,
        )),
    }
}

pub(crate) fn failed_without_requests(replay: &ReplayBatch) -> Result<Vec<Value>> {
    failures::collect(replay)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolContext {
    run_id: String,
    permission_id: String,
    original_request_sha256: String,
    permission_seq: u64,
    snapshot_seq: u64,
    declarations: Vec<SessionEvent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    post_request_declarations: Vec<SessionEvent>,
}

pub(crate) fn capture(
    permission: &PendingPermissionSnapshot,
    run_id: &str,
    replay: &ReplayBatch,
) -> Result<Vec<u8>> {
    let original = serde_json::to_vec(permission)?;
    if !replay.history_complete || replay.first_retained_seq != 1 {
        return unavailable(permission, "Tool history is incomplete");
    }
    let mut request_seq = None;
    for (index, event) in replay.events.iter().enumerate() {
        if event.seq != u64::try_from(index)?.saturating_add(1)
            || matches!(event.kind, SessionEventKind::PayloadOmitted { .. })
        {
            return unavailable(
                permission,
                "Tool history is discontinuous or omits material",
            );
        }
        if let SessionEventKind::Permission {
            permission: observed,
        } = &event.kind
            && observed.permission_id == permission.permission_id
        {
            if request_seq.is_some() || serde_json::to_vec(observed)? != original {
                return unavailable(
                    permission,
                    "Permission history has conflicting original requests",
                );
            }
            request_seq = Some(event.seq);
        }
    }
    if replay.events.last().map(|event| event.seq) != Some(replay.snapshot_seq) {
        return unavailable(
            permission,
            "Tool history snapshot does not match its events",
        );
    }
    let Some(permission_seq) = request_seq else {
        return unavailable(
            permission,
            "Original permission is absent from tool history",
        );
    };
    let id = permission.tool_call.tool_call_id.0.as_ref();
    let mut declarations = Vec::new();
    let mut post_request_declarations = Vec::new();
    for event in &replay.events {
        if let Some(mut value) = declaration(event)?
            && value["toolCallId"] == id
        {
            value
                .as_object_mut()
                .context("tool declaration is not an object")?
                .retain(|key, _| {
                    matches!(
                        key.as_str(),
                        "sessionUpdate"
                            | "toolCallId"
                            | "title"
                            | "kind"
                            | "status"
                            | "locations"
                            | "rawInput"
                    )
                });
            let mut event = event.clone();
            event.kind = SessionEventKind::Update {
                update: serde_json::from_value(value)?,
            };
            if event.seq < permission_seq {
                declarations.push(event);
            } else if event.seq > permission_seq {
                post_request_declarations.push(event);
            }
        }
    }
    if declarations.is_empty() && post_request_declarations.is_empty() {
        return Ok(original);
    }
    if declarations.len() + post_request_declarations.len() > MAX_DECLARATIONS {
        return unavailable(
            permission,
            "Tool declarations exceed the context count bound",
        );
    }
    let context = ToolContext {
        run_id: run_id.into(),
        permission_id: permission.permission_id.clone(),
        original_request_sha256: hex::encode(Sha256::digest(&original)),
        permission_seq,
        snapshot_seq: replay.snapshot_seq,
        declarations,
        post_request_declarations,
    };
    if serde_json::to_vec(&context)?.len() > MAX_CONTEXT_BYTES {
        return unavailable(
            permission,
            "Tool declarations exceed the context byte bound",
        );
    }
    let mut material = serde_json::to_value(permission)?;
    material
        .as_object_mut()
        .context("permission request is not an object")?
        .insert(CONTEXT_FIELD.into(), serde_json::to_value(context)?);
    Ok(serde_json::to_vec(&material)?)
}

pub(crate) fn unavailable(permission: &PendingPermissionSnapshot, reason: &str) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(permission)?;
    value["tool_context_unavailable"] = reason.into();
    Ok(serde_json::to_vec(&value)?)
}

pub(crate) fn original_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&serde_json::from_slice::<
        PendingPermissionSnapshot,
    >(bytes)?)?)
}

pub(crate) fn checked_value(bytes: &[u8], run_id: Option<&str>) -> Result<Value> {
    let value: Value = serde_json::from_slice(bytes)?;
    if value.get("native").is_some() {
        anyhow::ensure!(
            run_id.is_none(),
            "native material cannot identify an ACP run"
        );
        return serde_json::to_value(native::Request::decode(bytes)?)
            .context("serialize native permission");
    }
    let request: PendingPermissionSnapshot = serde_json::from_slice(bytes)?;
    if let Some(reason) = value.get("tool_context_unavailable") {
        anyhow::ensure!(
            reason
                .as_str()
                .is_some_and(|text| !text.is_empty() && text.len() <= 256)
                && value.get(CONTEXT_FIELD).is_none(),
            "invalid unavailable tool context"
        );
    }
    if let Some(context) = value.get(CONTEXT_FIELD) {
        anyhow::ensure!(
            serde_json::to_vec(context)?.len() <= MAX_CONTEXT_BYTES,
            "permission tool context exceeds its bound"
        );
        let context: ToolContext = serde_json::from_value(context.clone())?;
        anyhow::ensure!(
            !context.run_id.is_empty()
                && run_id.is_none_or(|id| id == context.run_id)
                && context.permission_id == request.permission_id
                && context.original_request_sha256
                    == hex::encode(Sha256::digest(original_bytes(bytes)?))
                && context.snapshot_seq >= context.permission_seq
                && (!context.declarations.is_empty()
                    || !context.post_request_declarations.is_empty())
                && context.declarations.len() + context.post_request_declarations.len()
                    <= MAX_DECLARATIONS,
            "permission tool context does not match its original request/run"
        );
        let mut previous = 0;
        for event in &context.declarations {
            anyhow::ensure!(
                event.seq > previous
                    && event.seq < context.permission_seq
                    && declaration(event)?
                        .is_some_and(|value| value["toolCallId"]
                            == request.tool_call.tool_call_id.0.as_ref()),
                "permission context is late, unordered or refers to another tool"
            );
            previous = event.seq;
        }
        previous = context.permission_seq;
        for event in &context.post_request_declarations {
            anyhow::ensure!(
                event.seq > previous
                    && event.seq <= context.snapshot_seq
                    && declaration(event)?
                        .is_some_and(|value| value["toolCallId"]
                            == request.tool_call.tool_call_id.0.as_ref()),
                "post-request context is unordered or refers to another tool"
            );
            previous = event.seq;
        }
    }
    Ok(value)
}

#[cfg(test)]
fn effective_tool(bytes: &[u8], run_id: &str) -> Result<Option<Value>> {
    effective_tool_from_checked(&checked_value(bytes, Some(run_id))?)
}

/// Inspect a denied operation's declared scope after a failed terminal update.
/// This never admits permission; recovery separately requires failure evidence.
pub(crate) fn denied_tool_scope(bytes: &[u8], run_id: &str) -> Result<Option<Value>> {
    tool_scope(&checked_value(bytes, Some(run_id))?, true)
}

/// Project only material already validated by checked_value or decision_value.
pub(crate) fn effective_tool_from_checked(material: &Value) -> Result<Option<Value>> {
    tool_scope(material, false)
}

fn tool_scope(material: &Value, failed_terminal: bool) -> Result<Option<Value>> {
    if material.get("tool_context_unavailable").is_some() {
        return Ok(None);
    }
    // Scope validation must see the stored declaration, including malformed
    // optional fields a tolerant SDK decoder might otherwise discard.
    let Some(mut tool) = material
        .get("tool_call")
        .filter(|tool| tool.is_object())
        .cloned()
    else {
        return Ok(None);
    };
    if matches!(tool["status"].as_str(), Some("completed" | "failed")) {
        return Ok(None);
    }
    let Some(context) = material.get(CONTEXT_FIELD) else {
        return Ok(Some(tool));
    };
    let context: ToolContext = serde_json::from_value(context.clone())?;
    let offered_arguments = tool["rawInput"].clone();
    let mut starts = 0;
    for event in &context.declarations {
        let Some(value) = declaration(event)? else {
            return Ok(None);
        };
        starts += usize::from(value["sessionUpdate"] == "tool_call");
        if starts > 1 || matches!(value["status"].as_str(), Some("completed" | "failed")) {
            return Ok(None);
        }
        if let Some(kind) = value.get("kind").filter(|value| !value.is_null()) {
            if tool
                .get("kind")
                .is_some_and(|known| !known.is_null() && known != kind)
            {
                return Ok(None);
            }
            tool["kind"] = kind.clone();
        }
        if let Some(locations) = value.get("locations").and_then(Value::as_array) {
            if tool.get("locations").is_none_or(Value::is_null) {
                tool["locations"] = serde_json::json!([]);
            }
            let Some(known) = tool["locations"].as_array_mut() else {
                return Ok(None);
            };
            for location in locations {
                if !known.contains(location) {
                    known.push(location.clone());
                }
            }
            if known.len() > MAX_DECLARATIONS {
                return Ok(None);
            }
        }
        if let Some(input) = value.get("rawInput").filter(|value| !value.is_null()) {
            if tool.get("rawInput").is_none_or(Value::is_null) {
                tool["rawInput"] = serde_json::json!({});
            }
            if let (Some(known), Some(supplied)) =
                (tool["rawInput"].as_object_mut(), input.as_object())
            {
                for (key, value) in supplied {
                    // Before permission, streamed content arguments may be
                    // replaced. Preserve the actual offered request and all
                    // earlier path declarations; content is not scope proof.
                    if !matches!(key.as_str(), "path" | "file_path" | "filePath") {
                        if offered_arguments.get(key).is_none() {
                            known.insert(key.clone(), value.clone());
                        }
                        continue;
                    }
                    if known.get(key).is_some_and(|known| known != value) {
                        return Ok(None);
                    }
                    known.insert(key.clone(), value.clone());
                }
            } else {
                return Ok(None);
            }
        }
    }
    if starts != 1 {
        return Ok(None);
    }
    for event in &context.post_request_declarations {
        let Some(value) = declaration(event)? else {
            return Ok(None);
        };
        if value["sessionUpdate"] == "tool_call"
            || value["status"].as_str() == Some("completed")
            || !failed_terminal && value["status"].as_str() == Some("failed")
        {
            return Ok(None);
        }
        if value
            .get("kind")
            .is_some_and(|kind| !kind.is_null() && tool.get("kind") != Some(kind))
        {
            return Ok(None);
        }
        if let Some(locations) = value.get("locations").and_then(Value::as_array)
            && !locations.iter().all(|location| {
                tool["locations"]
                    .as_array()
                    .is_some_and(|known| known.contains(location))
            })
        {
            return Ok(None);
        }
        if let Some(input) = value.get("rawInput").filter(|input| !input.is_null()) {
            let same = match (tool["rawInput"].as_object(), input.as_object()) {
                (Some(known), Some(input)) => input
                    .iter()
                    .all(|(key, value)| known.get(key) == Some(value)),
                _ => tool.get("rawInput") == Some(input),
            };
            if !same {
                return Ok(None);
            }
        }
    }
    Ok(Some(tool))
}

pub(crate) fn validate_current(saved: &[u8], current: &[u8], run_id: &str) -> Result<()> {
    anyhow::ensure!(
        original_bytes(saved)? == original_bytes(current)?,
        "original permission request changed before delivery"
    );
    let mut saved = checked_value(saved, Some(run_id))?;
    let mut current = checked_value(current, Some(run_id))?;
    let saved_tool = effective_tool_from_checked(&saved)?;
    let current_tool = effective_tool_from_checked(&current)?;
    if saved_tool.is_some() {
        anyhow::ensure!(
            saved_tool == current_tool,
            "declared tool scope or arguments changed before permission delivery"
        );
    } else {
        for value in [&mut saved, &mut current] {
            if let Some(context) = value.get_mut(CONTEXT_FIELD).and_then(Value::as_object_mut) {
                context.remove("snapshot_seq");
            }
        }
        anyhow::ensure!(
            saved == current,
            "unresolved permission context changed before delivery"
        );
    }
    Ok(())
}

fn declaration(event: &SessionEvent) -> Result<Option<Value>> {
    let SessionEventKind::Update { update } = &event.kind else {
        return Ok(None);
    };
    let value = serde_json::to_value(update)?;
    Ok(matches!(
        value["sessionUpdate"].as_str(),
        Some("tool_call" | "tool_call_update")
    )
    .then_some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::SessionEventKind;
    use anyhow::Context;

    #[test]
    fn earlier_context_fills_missing_locations_without_rewriting_the_request() -> Result<()> {
        let replay: ReplayBatch = serde_json::from_str(include_str!(
            "../tests/fixtures/acp-permission-context.json"
        ))?;
        let permission = replay
            .events
            .iter()
            .find_map(|event| match &event.kind {
                SessionEventKind::Permission { permission }
                    if permission.tool_call.tool_call_id.0.as_ref()
                        == "call_OvWajlu9o82Qpw6IdMUPLXmM" =>
                {
                    Some(permission)
                }
                _ => None,
            })
            .context("captured package request missing")?;
        let original = serde_json::to_vec(permission)?;
        let material = capture(permission, "original-run", &replay)?;
        let restored: PendingPermissionSnapshot = serde_json::from_slice(&material)?;
        assert_eq!(serde_json::to_vec(&restored)?, original);
        assert_eq!(
            serde_json::to_value(&restored.tool_call)?["locations"],
            serde_json::json!([])
        );
        let tool =
            effective_tool(&material, "original-run")?.context("captured context missing")?;
        assert_eq!(
            tool["locations"][0]["path"],
            "/fixture/workspace/package.json"
        );
        assert_eq!(
            tool["rawInput"]["filePath"],
            "/fixture/workspace/package.json"
        );
        assert_eq!(tool["kind"], "read");
        validate_current(&material, &material, "original-run")?;
        Ok(())
    }

    #[test]
    fn pre_permission_content_updates_preserve_the_offered_request_and_scope() -> Result<()> {
        let mut value: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/acp-permission-context.json"
        ))?;
        let events = value["events"].as_array_mut().context("events missing")?;
        for event in events {
            let sequence = event["seq"].as_u64();
            if matches!(sequence, Some(64 | 65)) {
                event["kind"]["update"]["kind"] = "edit".into();
            }
            if sequence == Some(65) {
                event["kind"]["update"]["rawInput"]["old_string"] =
                    "normalizeWhitespace('\\u00a0東京');".into();
            }
            if sequence == Some(66) {
                let tool = &mut event["kind"]["permission"]["tool_call"];
                tool["kind"] = "edit".into();
                tool["rawInput"] = serde_json::json!({
                    "filePath":"/fixture/workspace/package.json",
                    "old_string":"normalizeWhitespace('\u{00a0}東京');",
                    "new_string":""
                });
            }
        }
        let replay: ReplayBatch = serde_json::from_value(value)?;
        let permission = replay
            .events
            .iter()
            .find_map(|event| match &event.kind {
                SessionEventKind::Permission { permission } if event.seq == 66 => Some(permission),
                _ => None,
            })
            .context("permission missing")?;
        let material = capture(permission, "run", &replay)?;
        let tool = effective_tool(&material, "run")?.context("valid offered scope missing")?;
        assert_eq!(
            tool["rawInput"]["old_string"],
            "normalizeWhitespace('\u{00a0}東京');"
        );
        assert_eq!(original_bytes(&material)?, serde_json::to_vec(permission)?);
        let mut later: Value = serde_json::from_slice(&material)?;
        let mut update = later[CONTEXT_FIELD]["declarations"][1].clone();
        update["seq"] = 73.into();
        update["kind"]["update"]["rawInput"]["old_string"] = "changed after permission".into();
        later[CONTEXT_FIELD]["snapshot_seq"] = 73.into();
        later[CONTEXT_FIELD]["post_request_declarations"] = serde_json::json!([update]);
        assert!(effective_tool(&serde_json::to_vec(&later)?, "run")?.is_none());
        assert!(validate_current(&material, &serde_json::to_vec(&later)?, "run").is_err());
        Ok(())
    }

    #[test]
    fn context_rejects_foreign_late_changed_or_contradictory_declarations() -> Result<()> {
        let replay: ReplayBatch = serde_json::from_str(include_str!(
            "../tests/fixtures/acp-permission-context.json"
        ))?;
        let permission = replay
            .events
            .iter()
            .find_map(|event| match &event.kind {
                SessionEventKind::Permission { permission } if event.seq == 66 => Some(permission),
                _ => None,
            })
            .context("original permission missing")?;
        let material = capture(permission, "original-run", &replay)?;
        assert!(effective_tool(&material, "foreign-run").is_err());
        for change in ["request", "late", "tool"] {
            let mut value: Value = serde_json::from_slice(&material)?;
            match change {
                "request" => value["tool_call"]["title"] = "changed request".into(),
                "late" => value[CONTEXT_FIELD]["declarations"][1]["seq"] = 66.into(),
                "tool" => {
                    value[CONTEXT_FIELD]["declarations"][1]["kind"]["update"]["toolCallId"] =
                        "another-tool".into()
                }
                _ => anyhow::bail!("unexpected fixture mutation"),
            }
            assert!(effective_tool(&serde_json::to_vec(&value)?, "original-run").is_err());
        }
        let mut value: Value = serde_json::from_slice(&material)?;
        value[CONTEXT_FIELD]["declarations"][0]["kind"]["update"]["kind"] = "execute".into();
        assert!(effective_tool(&serde_json::to_vec(&value)?, "original-run")?.is_none());
        let mut value: Value = serde_json::from_slice(&material)?;
        value[CONTEXT_FIELD]["declarations"][0]["kind"]["update"]["rawInput"] =
            serde_json::json!({"filePath":"/outside/private"});
        assert!(effective_tool(&serde_json::to_vec(&value)?, "original-run")?.is_none());
        let mut incomplete = replay.clone();
        incomplete.history_complete = false;
        let incomplete = capture(permission, "original-run", &incomplete)?;
        assert_eq!(
            original_bytes(&incomplete)?,
            serde_json::to_vec(permission)?
        );
        assert!(effective_tool(&incomplete, "original-run")?.is_none());
        Ok(())
    }

    #[test]
    fn delivery_rejects_scope_widening_reused_calls_and_late_argument_changes() -> Result<()> {
        let replay: ReplayBatch = serde_json::from_str(include_str!(
            "../tests/fixtures/acp-permission-context.json"
        ))?;
        let permission = replay
            .events
            .iter()
            .find_map(|event| match &event.kind {
                SessionEventKind::Permission { permission } if event.seq == 66 => Some(permission),
                _ => None,
            })
            .context("original permission missing")?;
        let saved = capture(permission, "run", &replay)?;
        for mutation in ["path", "arguments", "reuse", "terminal"] {
            let mut value: Value = serde_json::from_slice(&saved)?;
            let mut event = value[CONTEXT_FIELD]["declarations"][1].clone();
            event["seq"] = 73.into();
            match mutation {
                "path" => {
                    event["kind"]["update"]["locations"] =
                        serde_json::json!([{"path":"/outside/private"}])
                }
                "arguments" => {
                    event["kind"]["update"]["rawInput"]["filePath"] = "/outside/private".into()
                }
                "reuse" => event["kind"]["update"]["sessionUpdate"] = "tool_call".into(),
                "terminal" => event["kind"]["update"]["status"] = "completed".into(),
                _ => anyhow::bail!("unexpected fixture mutation"),
            }
            value[CONTEXT_FIELD]["snapshot_seq"] = 73.into();
            value[CONTEXT_FIELD]["post_request_declarations"] = serde_json::json!([event]);
            let changed = serde_json::to_vec(&value)?;
            assert!(effective_tool(&changed, "run")?.is_none(), "{mutation}");
            assert!(
                validate_current(&saved, &changed, "run").is_err(),
                "{mutation}"
            );
        }
        let mut later: Value = serde_json::from_slice(&saved)?;
        later[CONTEXT_FIELD]["snapshot_seq"] = 80.into();
        validate_current(&saved, &serde_json::to_vec(&later)?, "run")?;
        Ok(())
    }

    #[test]
    fn oversized_or_late_only_context_cannot_fall_back_to_automatic_approval() -> Result<()> {
        let replay: ReplayBatch = serde_json::from_str(include_str!(
            "../tests/fixtures/acp-permission-context.json"
        ))?;
        let permission = replay
            .events
            .iter()
            .find_map(|event| match &event.kind {
                SessionEventKind::Permission { permission } if event.seq == 66 => Some(permission),
                _ => None,
            })
            .context("original permission missing")?;
        let mut huge = serde_json::to_value(&replay)?;
        huge["events"][64]["kind"]["update"]["rawInput"]["largeDeclaration"] =
            "x".repeat(MAX_CONTEXT_BYTES).into();
        let huge: ReplayBatch = serde_json::from_value(huge)?;
        let bytes = capture(permission, "run", &huge)?;
        assert!(effective_tool(&bytes, "run")?.is_none());
        assert_eq!(original_bytes(&bytes)?, serde_json::to_vec(permission)?);
        let mut late: Value = serde_json::from_slice(&capture(permission, "run", &replay)?)?;
        let mut event = late[CONTEXT_FIELD]["declarations"][1].clone();
        event["seq"] = 73.into();
        late[CONTEXT_FIELD]["declarations"] = serde_json::json!([]);
        late[CONTEXT_FIELD]["post_request_declarations"] = serde_json::json!([event]);
        late[CONTEXT_FIELD]["snapshot_seq"] = 73.into();
        assert!(effective_tool(&serde_json::to_vec(&late)?, "run")?.is_none());
        Ok(())
    }
}
