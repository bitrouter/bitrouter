//! Session-scoped native lifecycle hooks supplement transcript reconciliation.
//! Hooks record facts and never decide whether an agent action is permitted.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::types::MAX_RECORD_BYTES;

const EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "StopFailure",
    "PreCompact",
    "PostCompact",
];

/// Claude SDK accepts a JSON settings object, including CLI command hooks.
/// This adds invocation-local hooks and preserves all existing settings.
/// <https://code.claude.com/docs/en/agent-sdk/typescript>
/// <https://code.claude.com/docs/en/hooks>
pub async fn instrument(
    mut params: Value,
    spool: &Path,
    executable: &Path,
    fallback: Option<&Value>,
) -> Result<Value> {
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from);
    let meta = params
        .as_object_mut()
        .context("session params must be an object")?
        .entry("_meta")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("session meta must be an object")?
        .entry("claudeCode")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Claude meta must be an object")?;
    super::claude_sdk::instrument(meta)?;
    let options = meta
        .entry("options")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Claude options must be an object")?;
    let existing = options
        .remove("settings")
        .filter(|value| !value.is_null())
        .or_else(|| fallback.cloned())
        .unwrap_or_else(|| json!({}));
    let mut settings = if let Some(path) = existing.as_str() {
        let path = std::path::PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            cwd.context("settings path requires cwd")?.join(path)
        };
        let file = tokio::fs::File::open(path).await?;
        let mut bytes = vec![];
        file.take(MAX_RECORD_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() <= MAX_RECORD_BYTES,
            "session settings exceed size limit"
        );
        serde_json::from_slice(&bytes)?
    } else {
        existing
    };
    let hooks = settings
        .as_object_mut()
        .context("settings must be an object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("settings hooks must be an object")?;
    let command = executable
        .to_str()
        .context("hook executable must be UTF-8")?;
    let spool = spool.to_str().context("hook spool must be UTF-8")?;
    for event in EVENTS {
        let entries = hooks
            .entry(*event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("hook matchers must be an array")?;
        let own = json!({"hooks":[{"type":"command","command":command,"args":["native-session-hook","--spool",spool],"timeout":10}]});
        if !entries.contains(&own) {
            entries.push(own);
        }
    }
    options.insert("settings".into(), settings);
    options
        .entry("forwardSubagentText")
        .or_insert(Value::Bool(true));
    Ok(params)
}

/// A small process invoked by Claude after a native lifecycle event. The spool
/// is registered by the parent application and contains only scoped metadata.
pub async fn run(spool: &Path) -> Result<()> {
    ensure!(spool.is_absolute(), "hook spool must be absolute");
    let mut bytes = vec![];
    tokio::io::stdin()
        .take(MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= MAX_RECORD_BYTES,
        "native hook input exceeds size limit"
    );
    let raw: Value = serde_json::from_slice(&bytes)?;
    let event = raw
        .get("hook_event_name")
        .and_then(Value::as_str)
        .context("hook event name missing")?;
    ensure!(EVENTS.contains(&event), "unsupported native hook event");
    let mut payload = serde_json::Map::new();
    for key in [
        "hook_event_name",
        "session_id",
        "agent_id",
        "agent_type",
        "parent_agent_id",
        "prompt_id",
        "stop_hook_active",
        "background_tasks",
        "session_crons",
        "cwd",
        "transcript_path",
        "agent_transcript_path",
        "source",
        "trigger",
        "reason",
    ] {
        if let Some(value) = raw.get(key) {
            payload.insert(key.into(), value.clone());
        }
    }
    tokio::fs::create_dir_all(spool).await?;
    let path = spool.join(format!("hook-{}.jsonl", uuid::Uuid::new_v4()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    let mut bytes = serde_json::to_vec(
        &json!({"method":event,"phase":"hook","payload":payload,"observed_at":chrono::Utc::now().to_rfc3339()}),
    )?;
    bytes.push(b'\n');
    file.write_all(&bytes).await?;
    file.sync_all().await?;
    // Empty stdout and exit 0 leave Claude's normal lifecycle unchanged.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn instrumentation_preserves_settings_and_is_idempotent() -> Result<()> {
        let params = json!({"cwd":"/workspace","_meta":{"other":true,"claudeCode":{"options":{"settings":{"env":{"EXISTING":"kept"},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"existing-hook"}]}]}}}}}});
        let prepared = instrument(
            params,
            Path::new("/tmp/session spool"),
            Path::new("/opt/bin/bitrouter"),
            None,
        )
        .await?;
        assert_eq!(
            prepared.pointer("/_meta/claudeCode/options/settings/env/EXISTING"),
            Some(&json!("kept"))
        );
        assert_eq!(
            prepared
                .pointer("/_meta/claudeCode/options/settings/hooks/Stop")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(
            instrument(
                prepared.clone(),
                Path::new("/tmp/session spool"),
                Path::new("/opt/bin/bitrouter"),
                None,
            )
            .await?,
            prepared
        );
        Ok(())
    }
}
