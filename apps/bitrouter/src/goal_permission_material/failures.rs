//! Failed calls with no offered permission are not permission grants or no-effect proof.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn collect(replay: &ReplayBatch) -> Result<Vec<Value>> {
    anyhow::ensure!(
        replay.history_complete && replay.first_retained_seq == 1,
        "failed tool history is incomplete"
    );
    let requested = replay
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::Permission { permission } => {
                Some(permission.tool_call.tool_call_id.0.to_string())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut tools = BTreeMap::<String, Value>::new();
    let mut sequence = 0_u64;
    for event in &replay.events {
        anyhow::ensure!(
            event.seq == sequence.saturating_add(1)
                && !matches!(event.kind, SessionEventKind::PayloadOmitted { .. }),
            "failed tool history is discontinuous or omitted"
        );
        sequence = event.seq;
        let Some(update) = declaration(event)? else {
            continue;
        };
        let id = update["toolCallId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("failed tool identity missing")?;
        if requested.contains(id) {
            continue;
        }
        if update["sessionUpdate"] == "tool_call" {
            anyhow::ensure!(!tools.contains_key(id), "failed tool identity reused");
            anyhow::ensure!(
                matches!(update["kind"].as_str(), Some("read" | "edit" | "search")),
                "unrequested tool has unknown or nonlocal kind"
            );
            tools.insert(id.into(), serde_json::json!({"toolCallId":id,"kind":update["kind"],"locations":[],"rawInput":{}}));
            anyhow::ensure!(
                tools.len() <= MAX_DECLARATIONS,
                "failed tool declarations exceed bound"
            );
        }
        let tool = tools
            .get_mut(id)
            .context("failed tool update precedes its declaration")?;
        if let Some(kind) = update.get("kind").filter(|value| !value.is_null()) {
            anyhow::ensure!(
                *kind == tool["kind"],
                "unrequested failed tool kind changed"
            );
        }
        if let Some(status) = update["status"].as_str() {
            anyhow::ensure!(
                !matches!(tool["status"].as_str(), Some("completed" | "failed"))
                    || tool["status"] == status,
                "failed tool terminal state changed"
            );
            tool["status"] = status.into();
        }
        if let Some(value) = update.get("locations").filter(|value| !value.is_null()) {
            let supplied = value
                .as_array()
                .context("failed tool locations malformed")?;
            let known = tool["locations"]
                .as_array_mut()
                .context("failed tool locations missing")?;
            for location in supplied {
                if !known.contains(location) {
                    known.push(location.clone());
                }
            }
            anyhow::ensure!(
                known.len() <= MAX_DECLARATIONS,
                "failed tool scope exceeds bound"
            );
        }
        if let Some(value) = update.get("rawInput").filter(|value| !value.is_null()) {
            let supplied = value
                .as_object()
                .context("failed tool arguments malformed")?;
            let known = tool["rawInput"]
                .as_object_mut()
                .context("failed tool arguments missing")?;
            for (key, value) in supplied {
                anyhow::ensure!(
                    known.get(key).is_none_or(|old| old == value),
                    "unrequested failed tool arguments changed"
                );
                known.insert(key.clone(), value.clone());
            }
        }
    }
    anyhow::ensure!(
        sequence == replay.snapshot_seq,
        "failed tool cutoff changed"
    );
    anyhow::ensure!(
        tools.values().all(|tool| tool["status"] == "failed"),
        "unrequested tool has no failed terminal evidence"
    );
    Ok(tools.into_values().collect())
}
