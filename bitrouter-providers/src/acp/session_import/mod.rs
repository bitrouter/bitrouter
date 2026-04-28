//! Discover existing on-disk sessions for ACP-capable agents.
//!
//! Each agent stores its sessions in a different layout under the
//! user's home directory; this module wraps them behind a uniform
//! [`scan_for_cwd`] so the TUI can offer to import existing
//! conversations into a fresh session via `session/load` (PR 9).
//!
//! Failure policy: per-file errors are silently skipped (logged at
//! debug). The scan never panics, never fails wholesale because a
//! single artifact is malformed. Callers receive whatever was
//! parseable.

use std::path::{Path, PathBuf};

mod claude;
mod codex;

/// Cap on title-hint length so the import modal renders cleanly.
/// 80 chars matches typical terminal widths after sidebar/padding.
const TITLE_HINT_MAX: usize = 80;

/// A session discovered on-disk for one agent.
///
/// `external_session_id` is the agent-native identifier — for ACP
/// agents this is the value passed to `session/load`. `last_active_at`
/// is unix seconds, used to sort the import modal's list with the
/// most-recent entry on top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSession {
    /// Registry id of the agent (e.g. `claude-code`, `codex`).
    pub agent_id: String,
    /// Agent-native session id. For Claude this is the `.jsonl`
    /// stem; for Codex it's `payload.id` from the rollout meta.
    pub external_session_id: String,
    /// First-user-message snippet, normalised and length-capped.
    /// `None` if no user-typed content could be recovered.
    pub title_hint: Option<String>,
    /// Mtime of the source artifact, in unix seconds.
    pub last_active_at: i64,
    /// Path to the source artifact on disk. Useful for debugging
    /// and for the modal's "imported from" display.
    pub source_path: PathBuf,
}

/// Discover sessions across all `agents` whose on-disk storage maps
/// them to `cwd`. Aggregation order across agents is unspecified;
/// each agent's entries are returned in mtime-descending order.
///
/// Unknown agent ids are silently skipped (callers may pass any
/// registered agent name, including those without a scanner).
///
/// `home` is the user's home directory. Passed in rather than read
/// from the environment so tests can point at a tempdir, and so this
/// crate doesn't have to take on a `dirs` dependency.
pub fn scan_for_cwd(home: &Path, cwd: &Path, agents: &[String]) -> Vec<DiscoveredSession> {
    let mut out = Vec::new();
    for agent_id in agents {
        match agent_id.as_str() {
            "claude-code" => out.extend(claude::scan(home, cwd)),
            "codex" => out.extend(codex::scan(home, cwd)),
            _ => {}
        }
    }
    out
}

/// Whitespace-collapse and length-cap a candidate title hint.
/// Returns `None` for blank input; caps at [`TITLE_HINT_MAX`] chars
/// with a trailing ellipsis.
pub(super) fn truncate_hint(s: &str) -> Option<String> {
    let trimmed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= TITLE_HINT_MAX {
        Some(trimmed)
    } else {
        let mut out: String = trimmed.chars().take(TITLE_HINT_MAX - 1).collect();
        out.push('…');
        Some(out)
    }
}

/// Mtime as unix seconds, defaulting to 0 on any IO/clock error.
pub(super) fn mtime_unix_seconds(path: &Path) -> i64 {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    let Ok(modified) = meta.modified() else {
        return 0;
    };
    match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn truncate_hint_short_text_passes_through() {
        assert_eq!(
            truncate_hint("refactor router"),
            Some("refactor router".to_string())
        );
    }

    #[test]
    fn truncate_hint_collapses_whitespace() {
        assert_eq!(
            truncate_hint("  hello\n\nworld  "),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn truncate_hint_blank_returns_none() {
        assert_eq!(truncate_hint(""), None);
        assert_eq!(truncate_hint("   \n\t"), None);
    }

    #[test]
    fn truncate_hint_long_text_ellipsis() {
        let long = "a".repeat(TITLE_HINT_MAX + 20);
        let hint = truncate_hint(&long).expect("non-empty");
        assert_eq!(hint.chars().count(), TITLE_HINT_MAX);
        assert!(hint.ends_with('…'));
    }

    #[test]
    fn scan_unknown_agent_returns_empty() {
        let home = TempDir::new().expect("tempdir");
        let cwd = std::path::PathBuf::from("/Users/x/proj");
        let result = scan_for_cwd(home.path(), &cwd, &["mystery-agent".to_string()]);
        assert!(result.is_empty());
    }

    #[test]
    fn scan_with_empty_home_returns_empty() {
        let home = TempDir::new().expect("tempdir");
        let cwd = std::path::PathBuf::from("/Users/x/proj");
        let result = scan_for_cwd(
            home.path(),
            &cwd,
            &["claude-code".to_string(), "codex".to_string()],
        );
        assert!(result.is_empty());
    }
}
