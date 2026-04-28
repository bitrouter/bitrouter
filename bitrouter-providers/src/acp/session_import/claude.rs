//! Scan `~/.claude/projects/<dashed-cwd>/*.jsonl` for prior sessions.
//!
//! Claude Code stores each session as a `.jsonl` file under a
//! per-project subdirectory. The subdirectory name is derived from
//! the absolute cwd path: leading `/` is stripped and remaining `/`
//! separators are replaced with `-`. The filename stem is the ACP
//! session id.
//!
//! Each line is one JSON object. We only need the first line that
//! looks like a real user prompt to derive a title hint, and the file
//! mtime for sorting. Other entries (tool calls, agent responses,
//! file snapshots) are skipped without parsing them in full.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde::Deserialize;

use super::{DiscoveredSession, mtime_unix_seconds, truncate_hint};

const AGENT_ID: &str = "claude-code";

/// Walk `~/.claude/projects/<dashed-cwd>/` and return one
/// [`DiscoveredSession`] per `*.jsonl` file. Empty result if the
/// directory doesn't exist (no prior sessions for this project).
pub(super) fn scan(home: &Path, cwd: &Path) -> Vec<DiscoveredSession> {
    let project_dir = home.join(".claude").join("projects").join(dashed(cwd));
    let entries = match fs::read_dir(&project_dir) {
        Ok(it) => it,
        Err(_) => return Vec::new(),
    };

    let mut out: Vec<DiscoveredSession> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(session) = parse_file(&path) {
            out.push(session);
        }
    }
    // Most-recent first.
    out.sort_by_key(|s| std::cmp::Reverse(s.last_active_at));
    out
}

/// Convert an absolute cwd to Claude's project subdir scheme:
/// `/Users/x/proj` → `-Users-x-proj`.
fn dashed(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    s.replace('/', "-")
}

fn parse_file(path: &Path) -> Option<DiscoveredSession> {
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let last_active_at = mtime_unix_seconds(path);

    // Streaming line read — sessions can be huge, but we only care
    // about the first prompt-bearing user line.
    let title_hint = first_user_prompt(path);

    Some(DiscoveredSession {
        agent_id: AGENT_ID.to_string(),
        external_session_id: stem,
        title_hint,
        last_active_at,
        source_path: path.to_path_buf(),
    })
}

fn first_user_prompt(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        let entry: ClaudeEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.entry_type.as_deref() != Some("user") {
            continue;
        }
        if entry.is_meta.unwrap_or(false) {
            continue;
        }
        let Some(content) = entry.message.and_then(|m| m.content_text()) else {
            continue;
        };
        if is_synthetic_prompt(&content) {
            continue;
        }
        return truncate_hint(&content);
    }
    None
}

/// Skip prompts injected by `/clear`, hook output, etc.
fn is_synthetic_prompt(content: &str) -> bool {
    let t = content.trim_start();
    t.starts_with("<command-name>")
        || t.starts_with("<local-command-stdout>")
        || t.starts_with("<local-command-caveat>")
        || t.starts_with("<system-reminder>")
        || t.starts_with("<environment_context>")
}

#[derive(Deserialize)]
struct ClaudeEntry {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    #[serde(rename = "isMeta", default)]
    is_meta: Option<bool>,
    message: Option<ClaudeMessage>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    #[serde(default)]
    content: Option<serde_json::Value>,
}

impl ClaudeMessage {
    /// Extract text from a Claude message's `content` field, which is
    /// either a plain string (older entries) or an array of blocks
    /// (newer entries with tool use). For the array form, we
    /// concatenate any `text`-typed blocks.
    fn content_text(self) -> Option<String> {
        match self.content? {
            serde_json::Value::String(s) => Some(s),
            serde_json::Value::Array(arr) => {
                let mut buf = String::new();
                for block in arr {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        if !buf.is_empty() {
                            buf.push(' ');
                        }
                        buf.push_str(t);
                    }
                }
                if buf.is_empty() { None } else { Some(buf) }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_session(home: &Path, cwd: &Path, session_id: &str, lines: &[&str]) -> PathBuf {
        let dir = home.join(".claude").join("projects").join(dashed(cwd));
        fs::create_dir_all(&dir).expect("mkdir fixture");
        let path = dir.join(format!("{session_id}.jsonl"));
        let mut f = fs::File::create(&path).expect("create fixture");
        for line in lines {
            writeln!(f, "{line}").expect("write fixture");
        }
        path
    }

    fn user_line(content: &str, is_meta: bool) -> String {
        format!(
            r#"{{"type":"user","isMeta":{is_meta},"message":{{"role":"user","content":{content}}}}}"#,
            content = serde_json::Value::String(content.to_string())
        )
    }

    #[test]
    fn dashed_strips_leading_slash() {
        let cwd = Path::new("/Users/x/Documents/Code/bitrouter");
        assert_eq!(dashed(cwd), "-Users-x-Documents-Code-bitrouter");
    }

    #[test]
    fn empty_project_dir_returns_empty() {
        let home = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from("/missing/proj");
        assert!(scan(home.path(), &cwd).is_empty());
    }

    #[test]
    fn non_jsonl_files_ignored() {
        let home = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from("/proj");
        let dir = home
            .path()
            .join(".claude")
            .join("projects")
            .join(dashed(&cwd));
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join("notes.txt"), "ignored").expect("write txt");
        assert!(scan(home.path(), &cwd).is_empty());
    }

    #[test]
    fn discovers_session_with_first_real_user_prompt() {
        let home = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from("/proj");
        // First "user" entry is meta (skipped); second is a synthetic
        // command (skipped); third is the real first prompt.
        let path = write_session(
            home.path(),
            &cwd,
            "abc-123",
            &[
                r#"{"type":"file-history-snapshot"}"#,
                &user_line("<local-command-caveat>nope</local-command-caveat>", true),
                &user_line("<command-name>/clear</command-name>", false),
                &user_line("Refactor the router please", false),
                &user_line("(should not be picked)", false),
            ],
        );
        let sessions = scan(home.path(), &cwd);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.agent_id, AGENT_ID);
        assert_eq!(s.external_session_id, "abc-123");
        assert_eq!(s.title_hint.as_deref(), Some("Refactor the router please"));
        assert_eq!(s.source_path, path);
    }

    #[test]
    fn malformed_line_does_not_skip_subsequent_real_prompt() {
        let home = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from("/proj");
        write_session(
            home.path(),
            &cwd,
            "id1",
            &[
                "this is not json at all",
                &user_line("Real prompt here", false),
            ],
        );
        let sessions = scan(home.path(), &cwd);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title_hint.as_deref(), Some("Real prompt here"));
    }

    #[test]
    fn session_with_no_prompt_has_none_title() {
        let home = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from("/proj");
        write_session(
            home.path(),
            &cwd,
            "id2",
            &[r#"{"type":"file-history-snapshot"}"#],
        );
        let sessions = scan(home.path(), &cwd);
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].title_hint.is_none());
    }

    #[test]
    fn discovers_multiple_sessions_in_a_project() {
        let home = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from("/proj");
        write_session(home.path(), &cwd, "first", &[&user_line("A", false)]);
        write_session(home.path(), &cwd, "second", &[&user_line("B", false)]);

        let sessions = scan(home.path(), &cwd);
        assert_eq!(sessions.len(), 2);
        let ids: Vec<&str> = sessions
            .iter()
            .map(|s| s.external_session_id.as_str())
            .collect();
        assert!(ids.contains(&"first"));
        assert!(ids.contains(&"second"));
    }

    #[test]
    fn array_content_concatenates_text_blocks() {
        let home = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from("/proj");
        // Newer Claude entries use array-style content.
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hello"},{"type":"text","text":"world"}]}}"#;
        write_session(home.path(), &cwd, "id3", &[line]);
        let sessions = scan(home.path(), &cwd);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title_hint.as_deref(), Some("hello world"));
    }
}
