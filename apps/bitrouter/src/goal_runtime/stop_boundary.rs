//! Wait before actuation; this never certifies a tool's effects or permits replay.
use super::*;
use crate::supervisor::{ReplayBatch, RunSnapshot, SessionEventKind, TurnState};

impl Coordinator {
    pub(super) async fn wait_for_stop_boundary(&self, run_id: &str) -> Result<bool> {
        let response = self
            .supervisor
            .dispatch(SessionRequest::authorized(
                &self.grant,
                SessionCommand::Snapshot {
                    run_id: run_id.into(),
                },
            ))
            .await?;
        let SessionResponse::Snapshot { snapshot } = response else {
            anyhow::bail!("stop-boundary snapshot unavailable")
        };
        if snapshot.owned_process_terminated() || snapshot.turn == TurnState::Idle {
            return Ok(false);
        }
        if snapshot.pending_permissions.is_empty() {
            return Ok(true);
        }
        let response = self
            .supervisor
            .dispatch(SessionRequest::authorized(
                &self.grant,
                SessionCommand::Events {
                    run_id: run_id.into(),
                    after_seq: 0,
                },
            ))
            .await?;
        let SessionResponse::Events { replay } = response else {
            anyhow::bail!("stop-boundary history unavailable")
        };
        Ok(!permission_boundary(&snapshot, &replay))
    }
}

fn permission_boundary(snapshot: &RunSnapshot, replay: &ReplayBatch) -> bool {
    if snapshot.pending_permissions.is_empty()
        || !replay.history_complete
        || replay.first_retained_seq != 1
        || replay.snapshot_seq != snapshot.last_seq
    {
        return false;
    }
    let mut permissions = BTreeSet::new();
    let mut tools = BTreeMap::new();
    let mut last_seq = 0_u64;
    for event in &replay.events {
        if last_seq.checked_add(1) != Some(event.seq) {
            return false;
        }
        last_seq = event.seq;
        match &event.kind {
            SessionEventKind::Permission { permission } => {
                permissions.insert(permission.tool_call.tool_call_id.0.to_string());
            }
            SessionEventKind::PayloadOmitted { .. } => return false,
            SessionEventKind::Update { update } => {
                let Ok(value) = serde_json::to_value(update) else {
                    return false;
                };
                if !matches!(
                    value["sessionUpdate"].as_str(),
                    Some("tool_call" | "tool_call_update")
                ) {
                    continue;
                }
                let Some(id) = value["toolCallId"].as_str() else {
                    return false;
                };
                let status = tools
                    .entry(id.to_owned())
                    .or_insert_with(|| "pending".to_owned());
                if let Some(actual) = value["status"].as_str() {
                    *status = actual.to_owned();
                }
            }
            _ => {}
        }
    }
    last_seq == snapshot.last_seq
        && tools.iter().all(|(id, status)| {
            matches!(status.as_str(), "completed" | "failed") || permissions.contains(id)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Result<(RunSnapshot, serde_json::Value)> {
        let permission = serde_json::json!({"permission_id":"p","tool_call":{"toolCallId":"read","title":"Read","kind":"read","locations":[{"path":"input.txt"}]},"options":[]});
        let snapshot = serde_json::from_value(serde_json::json!({
            "run_id":"run","label":"test","agent_id":"peer","cwd":"/work","claim_key":"/work",
            "presentation":"background","process":"running","turn":"working","attention":"permission",
            "attachment":{"state":"detached"},"review":"unread","activity":"waiting","pending_permissions":[permission],
            "last_seq":3,"started_at":"2026-10-15T00:00:00Z","updated_at":"2026-10-15T00:00:01Z"
        }))?;
        let replay = serde_json::json!({"first_retained_seq":1,"snapshot_seq":3,"history_complete":true,"events":[
            {"seq":1,"at":"2026-10-15T00:00:00Z","kind":{"event":"update","update":{"sessionUpdate":"tool_call","toolCallId":"read","title":"Read","kind":"read"}}},
            {"seq":2,"at":"2026-10-15T00:00:00Z","kind":{"event":"permission","permission":permission}},
            {"seq":3,"at":"2026-10-15T00:00:00Z","kind":{"event":"update","update":{"sessionUpdate":"tool_call","toolCallId":"streaming-write","title":"Preparing file","kind":"edit"}}}
        ]});
        Ok((snapshot, replay))
    }

    #[test]
    fn an_announced_write_without_its_permission_defers_cancellation() -> Result<()> {
        let (mut snapshot, mut value) = sample()?;
        assert!(!permission_boundary(
            &snapshot,
            &serde_json::from_value(value.clone())?
        ));
        let events = value["events"].as_array_mut().context("events missing")?;
        events.push(serde_json::json!({"seq":4,"at":"2026-10-15T00:00:01Z","kind":{"event":"permission","permission":{
            "permission_id":"write-request","tool_call":{"toolCallId":"streaming-write","title":"Write","kind":"edit","locations":[{"path":"text.mjs"}]},"options":[]}}}));
        value["snapshot_seq"] = 4.into();
        snapshot.last_seq = 4;
        let replay: ReplayBatch = serde_json::from_value(value.clone())?;
        assert!(permission_boundary(&snapshot, &replay));
        value["history_complete"] = false.into();
        assert!(!permission_boundary(
            &snapshot,
            &serde_json::from_value(value.clone())?
        ));
        value["history_complete"] = true.into();
        value["events"][1]["seq"] = 9.into();
        assert!(!permission_boundary(
            &snapshot,
            &serde_json::from_value(value)?
        ));
        snapshot.last_seq = 5;
        assert!(!permission_boundary(&snapshot, &replay));
        Ok(())
    }
}
