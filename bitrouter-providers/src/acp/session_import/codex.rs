//! Scan `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-*.jsonl` for
//! prior sessions matching this cwd.
//!
//! Codex stores all sessions globally (no per-cwd partitioning), so
//! each rollout's first line — `type: session_meta` — carries
//! `payload.cwd`. We open every rollout, read just that first line,
//! and keep the file only when its meta cwd matches `cwd`. The
//! agent-native session id is `payload.id`. Files whose first line
//! isn't well-formed `session_meta` are silently skipped.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde::Deserialize;

use super::{DiscoveredSession, mtime_unix_seconds, truncate_hint};

const AGENT_ID: &str = "codex";

pub(super) fn scan(home: &Path, cwd: &Path) -> Vec<DiscoveredSession> {
    let root = home.join(".codex").join("sessions");
    if !root.is_dir() {
        return Vec::new();
    }
    let mut out: Vec<DiscoveredSession> = Vec::new();
    walk_rollouts(&root, &mut out, cwd);
    out.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
    out
}

/// Recursively walk the `sessions/` tree (YYYY/MM/DD/) and parse each
/// `rollout-*.jsonl` we find. Per-file errors are silent.
fn walk_rollouts(dir: &Path, out: &mut Vec<DiscoveredSession>, cwd: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_rollouts(&path, out, cwd);
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
            continue;
        }
        if let Some(session) = parse_rollout(&path, cwd) {
            out.push(session);
        }
    }
}

fn parse_rollout(path: &Path, cwd: &Path) -> Option<DiscoveredSession> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    let meta: SessionMetaLine = serde_json::from_str(&first).ok()?;
    if meta.entry_type != "session_meta" {
        return None;
    }
    let payload = meta.payload?;
    if payload.cwd.as_deref().map(Path::new) != Some(cwd) {
        return None;
    }
    let title_hint = first_user_message(&mut reader);
    Some(DiscoveredSession {
        agent_id: AGENT_ID.to_string(),
        external_session_id: payload.id,
        title_hint,
        last_active_at: mtime_unix_seconds(path),
        source_path: path.to_path_buf(),
    })
}

/// Continue reading lines from `reader` (positioned past the meta
/// line) and return the first non-synthetic user message text.
fn first_user_message<R: BufRead>(reader: &mut R) -> Option<String> {
    for line in reader.lines().map_while(Result::ok) {
        let entry: ResponseItemLine = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.entry_type != "response_item" {
            continue;
        }
        let Some(payload) = entry.payload else {
            continue;
        };
        if payload.payload_type != "message" || payload.role.as_deref() != Some("user") {
            continue;
        }
        let text = payload.content.into_iter().flatten().find_map(|block| {
            if block.block_type.as_deref() == Some("input_text") {
                block.text
            } else {
                None
            }
        })?;
        if is_synthetic_prompt(&text) {
            continue;
        }
        return truncate_hint(&text);
    }
    None
}

fn is_synthetic_prompt(content: &str) -> bool {
    let t = content.trim_start();
    t.starts_with("<environment_context>") || t.starts_with("<user_instructions>")
}

#[derive(Deserialize)]
struct SessionMetaLine {
    #[serde(rename = "type")]
    entry_type: String,
    payload: Option<SessionMetaPayload>,
}

#[derive(Deserialize)]
struct SessionMetaPayload {
    id: String,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct ResponseItemLine {
    #[serde(rename = "type")]
    entry_type: String,
    payload: Option<ResponseItemPayload>,
}

#[derive(Deserialize)]
struct ResponseItemPayload {
    #[serde(rename = "type")]
    payload_type: String,
    role: Option<String>,
    content: Option<Vec<ContentBlock>>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: Option<String>,
    text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_rollout(home: &Path, day: &str, name: &str, lines: &[&str]) -> PathBuf {
        let parts: Vec<&str> = day.split('/').collect();
        let mut dir = home.join(".codex").join("sessions");
        for p in &parts {
            dir = dir.join(p);
        }
        fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(name);
        let mut f = fs::File::create(&path).expect("create");
        for line in lines {
            writeln!(f, "{line}").expect("write");
        }
        path
    }

    fn meta_line(id: &str, cwd: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-04-04T03:24:41Z","type":"session_meta","payload":{{"id":{id},"cwd":{cwd}}}}}"#,
            id = serde_json::Value::String(id.to_string()),
            cwd = serde_json::Value::String(cwd.to_string())
        )
    }

    fn user_msg_line(text: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-04-04T03:24:42Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":{text}}}]}}}}"#,
            text = serde_json::Value::String(text.to_string())
        )
    }

    #[test]
    fn missing_sessions_dir_returns_empty() {
        let home = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from("/proj");
        assert!(scan(home.path(), &cwd).is_empty());
    }

    #[test]
    fn discovers_rollout_with_matching_cwd() {
        let home = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from("/proj");
        write_rollout(
            home.path(),
            "2026/04/03",
            "rollout-2026-04-03T23-24-38-019d-uuid.jsonl",
            &[
                &meta_line("019d-uuid", "/proj"),
                &user_msg_line("<environment_context>cwd=/proj</environment_context>"),
                &user_msg_line("hi codex"),
                &user_msg_line("(should not pick this)"),
            ],
        );
        let sessions = scan(home.path(), &cwd);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.agent_id, AGENT_ID);
        assert_eq!(s.external_session_id, "019d-uuid");
        assert_eq!(s.title_hint.as_deref(), Some("hi codex"));
    }

    #[test]
    fn rollout_with_other_cwd_skipped() {
        let home = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from("/proj");
        write_rollout(
            home.path(),
            "2026/04/03",
            "rollout-other.jsonl",
            &[&meta_line("uuid", "/somewhere/else")],
        );
        assert!(scan(home.path(), &cwd).is_empty());
    }

    #[test]
    fn non_rollout_file_ignored() {
        let home = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from("/proj");
        let dir = home.path().join(".codex").join("sessions").join("misc");
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join("notes.txt"), "stray").expect("write");
        assert!(scan(home.path(), &cwd).is_empty());
    }

    #[test]
    fn malformed_meta_line_skipped_silently() {
        let home = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from("/proj");
        write_rollout(
            home.path(),
            "2026/04/03",
            "rollout-bad.jsonl",
            &["this is not json"],
        );
        // Other rollout in the same tree still surfaces.
        write_rollout(
            home.path(),
            "2026/04/03",
            "rollout-good.jsonl",
            &[&meta_line("good", "/proj"), &user_msg_line("hello")],
        );
        let sessions = scan(home.path(), &cwd);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].external_session_id, "good");
    }

    #[test]
    fn rollout_with_no_user_msg_has_none_title() {
        let home = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from("/proj");
        write_rollout(
            home.path(),
            "2026/04/03",
            "rollout-empty.jsonl",
            &[&meta_line("u", "/proj")],
        );
        let sessions = scan(home.path(), &cwd);
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].title_hint.is_none());
    }
}
