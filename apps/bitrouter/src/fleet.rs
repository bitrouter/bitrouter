//! Shared fleet mechanics — one implementation for the two fleet owners that
//! exist until the fleet daemon ships (TUI_SPEC §2): the TUI's in-process
//! fleet (`tui::mod`) and the MCP bridge subprocess (`fleet_mcp`). Branch
//! naming, git integration verbs, and the cross-process `PORT` pool live
//! here so the two sides cannot drift; the daemon later wraps this module
//! instead of reconciling two copies.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bitrouter_substrate::translate::PermissionOutcome;
use bitrouter_substrate::up::PendingPermission;

/// Env var carrying the TUI's fleet-socket path. The TUI injects it into the
/// orchestrator's PTY environment; the MCP bridge subprocess (spawned by the
/// harness, which inherits that environment) connects back through it.
pub const TUI_SOCK_ENV: &str = "BITROUTER_FLEET_TUI_SOCK";

/// The per-repo fleet socket the TUI listens on (Unix only).
pub fn tui_sock_path(base_repo: &Path) -> PathBuf {
    base_repo.join(".bitrouter").join("fleet-tui.sock")
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 character —
/// a raw `String::truncate` at a fixed byte offset panics mid-character.
pub fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// One NDJSON message the MCP bridge sends the TUI over the fleet socket:
/// fleet mirroring (spawned/state/closed) plus gated permission requests the
/// bridge won't auto-resolve — those go to the human's decision queue
/// (TUI_SPEC §5's escalation home) instead of the old silent high-risk deny.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeMsg {
    Spawned {
        handle: String,
        agent: String,
        port: Option<u16>,
    },
    /// Task-shaped state change: `working` / `completed` / `failed`.
    State {
        handle: String,
        state: String,
    },
    Closed {
        handle: String,
    },
    Permission {
        /// Bridge-local id the TUI echoes back in `TuiMsg::Resolve`.
        id: u64,
        handle: String,
        title: String,
        diff: Option<WireDiff>,
        options: Vec<WireOption>,
    },
}

/// One NDJSON message the TUI sends the bridge.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TuiMsg {
    /// Handshake: the TUI's standing policy state at connect time.
    Hello { bootstrap_approved: bool },
    /// The human approved the worktree bootstrap hook (first-use confirm).
    BootstrapApproved,
    /// The human resolved permission `id` (`allow_once`/`allow_always`/`deny`).
    Resolve { id: u64, outcome: String },
}

/// A permission's file diff, reduced to wire data (both texts capped).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct WireDiff {
    pub path: String,
    pub old: String,
    pub new: String,
}

/// A permission option, reduced to display label + outcome keyword.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct WireOption {
    pub label: String,
    pub outcome: String,
}

/// Cap per side of a wire diff — big enough to review, small enough to
/// never stall the socket.
const WIRE_DIFF_CAP: usize = 64 * 1024;

/// Extract a pending permission's diff as wire data, texts capped.
pub fn wire_diff(p: &PendingPermission) -> Option<WireDiff> {
    use agent_client_protocol::schema::v1::ToolCallContent;
    p.tool_call
        .fields
        .content
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find_map(|c| match c {
            ToolCallContent::Diff(d) => {
                let mut old = d.old_text.clone().unwrap_or_default();
                let mut new = d.new_text.clone();
                truncate_utf8(&mut old, WIRE_DIFF_CAP);
                truncate_utf8(&mut new, WIRE_DIFF_CAP);
                Some(WireDiff {
                    path: d.path.display().to_string(),
                    old,
                    new,
                })
            }
            _ => None,
        })
}

/// Map a pending permission's options to display data, matching the TUI's
/// y/a/n handling (allow-once / allow-always / deny).
pub fn wire_options(p: &PendingPermission) -> Vec<WireOption> {
    use agent_client_protocol::schema::v1::PermissionOptionKind;
    p.options
        .iter()
        .map(|o| {
            let (outcome, label) = match o.kind {
                PermissionOptionKind::AllowOnce => (PermissionOutcome::AllowOnce, "allow"),
                PermissionOptionKind::AllowAlways => {
                    (PermissionOutcome::AllowAlways, "allow always")
                }
                // Any reject/unknown kind maps to Deny — the TUI offers y/a/n.
                _ => (PermissionOutcome::Deny, "deny"),
            };
            WireOption {
                label: label.to_string(),
                outcome: outcome_str(outcome).to_string(),
            }
        })
        .collect()
}

/// Wire keyword for an outcome (`TuiMsg::Resolve` / `WireOption`).
pub fn outcome_str(o: PermissionOutcome) -> &'static str {
    match o {
        PermissionOutcome::AllowOnce => "allow_once",
        PermissionOutcome::AllowAlways => "allow_always",
        PermissionOutcome::Deny => "deny",
    }
}

/// Parse a wire outcome keyword; anything unrecognized denies (fail-closed).
pub fn outcome_from_str(s: &str) -> PermissionOutcome {
    match s {
        "allow_once" => PermissionOutcome::AllowOnce,
        "allow_always" => PermissionOutcome::AllowAlways,
        _ => PermissionOutcome::Deny,
    }
}

/// Branch-safe agent tag: keep `[A-Za-z0-9._]`, everything else becomes `-`.
pub fn branch_tag(agent_id: &str) -> String {
    agent_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The 16-char handle derived from a session record id (`record16`) — the
/// same derivation the substrate uses for worktree/branch placeholders.
pub fn record16(record_id: &str) -> String {
    record_id.chars().filter(|c| *c != '-').take(16).collect()
}

/// The shared worktree/branch naming convention for fleet subagents:
/// `bitrouter/<tag>-<record16>`, retained on shutdown (cleanup is gated on
/// merged-or-discarded, never automatic — TUI_SPEC §6).
pub fn worktree_spec(tag: &str) -> bitrouter_substrate::worktree::WorktreeSpec {
    bitrouter_substrate::worktree::WorktreeSpec {
        name: format!("{tag}-{{record16}}"),
        branch: Some(format!("bitrouter/{tag}-{{record16}}")),
        remove_on_shutdown: false,
    }
}

/// The base repo `HEAD` at spawn — the diff/merge base. Best-effort: an
/// empty string outside a git repo (diffs then fail with git's own message).
pub async fn base_head(base_repo: &Path) -> String {
    git_stdout(base_repo, &["rev-parse", "HEAD"])
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Merge the subagent's branch into the base repo, keeping history
/// (`--no-ff`). Requires committed work; a dirty worktree fails with
/// `dirty_hint` appended (each caller words the fix for its own verbs).
pub async fn merge_branch(
    base_repo: &Path,
    worktree: &Path,
    branch: &str,
    dirty_hint: &str,
) -> Result<()> {
    let dirty = git_stdout(worktree, &["status", "--porcelain"]).await?;
    if !dirty.trim().is_empty() {
        anyhow::bail!("the worktree has uncommitted changes — {dirty_hint}");
    }
    let msg = format!("merge {branch}");
    git_ok(base_repo, &["merge", "--no-ff", "-m", &msg, branch]).await
}

/// Apply the subagent's diff vs its spawn base onto the base working tree,
/// uncommitted — the human writes the commit.
pub async fn apply_diff(base_repo: &Path, worktree: &Path, base_ref: &str) -> Result<()> {
    let patch = git_stdout(worktree, &["diff", "--binary", base_ref]).await?;
    if patch.trim().is_empty() {
        anyhow::bail!("nothing to apply: the diff vs the spawn base is empty");
    }
    git_apply(base_repo, &patch).await
}

/// `+adds/-dels/files` over the worktree vs its spawn base (tracked changes).
pub async fn diff_stat(worktree: &Path, base_ref: &str) -> Option<serde_json::Value> {
    let numstat = git_stdout(worktree, &["diff", "--numstat", base_ref])
        .await
        .ok()?;
    let (mut adds, mut dels, mut files) = (0u64, 0u64, 0u64);
    for line in numstat.lines() {
        let mut parts = line.split_whitespace();
        let a = parts.next()?.parse::<u64>().unwrap_or(0);
        let d = parts.next()?.parse::<u64>().unwrap_or(0);
        adds += a;
        dels += d;
        files += 1;
    }
    Some(serde_json::json!({ "files": files, "adds": adds, "dels": dels }))
}

/// Run git in `dir`, capturing stdout; errors carry stderr.
pub async fn git_stdout(dir: &Path, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .await
        .with_context(|| format!("spawning `git {}`", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run git in `dir` for effect only.
pub async fn git_ok(dir: &Path, args: &[&str]) -> Result<()> {
    git_stdout(dir, args).await.map(|_| ())
}

/// `git apply` the patch text onto `dir`'s working tree (3-way for context
/// drift; fails clean on conflicts).
pub async fn git_apply(dir: &Path, patch: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("git")
        .current_dir(dir)
        .args(["apply", "--3way"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning `git apply`")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(patch.as_bytes())
            .await
            .context("writing patch to `git apply`")?;
    }
    let out = child
        .wait_with_output()
        .await
        .context("waiting for `git apply`")?;
    if !out.status.success() {
        anyhow::bail!(
            "`git apply` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The `PORT` env overlay for a launch, when a port was leased.
pub fn port_env(port: Option<u16>) -> Vec<(String, String)> {
    port.map(|p| vec![("PORT".to_string(), p.to_string())])
        .unwrap_or_default()
}

/// An exclusive claim on one pool port, backed by a lease file under
/// `.bitrouter/ports/<port>`. Dropping the lease releases the port.
///
/// The file makes the pool **cross-process**: the TUI's fleet and any MCP
/// bridge subprocesses allocate from the same pool, so two fleets can no
/// longer hand two dev servers the same `PORT` (they previously each scanned
/// only their own registry).
pub struct PortLease {
    path: PathBuf,
    port: u16,
}

impl PortLease {
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for PortLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Claim the lowest free port in the inclusive pool; `None` when exhausted
/// (the agent then simply gets no `PORT`). Atomic across processes via
/// `O_EXCL` lease files; a lease whose recorded owner is dead (crash) is
/// reclaimed once, then re-contended.
pub fn reserve_port(base_repo: &Path, range: (u16, u16)) -> Option<PortLease> {
    let dir = base_repo.join(".bitrouter").join("ports");
    std::fs::create_dir_all(&dir).ok()?;
    for port in range.0..=range.1 {
        let path = dir.join(port.to_string());
        // Two attempts: the second only after reclaiming a stale lease.
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = write!(f, "{}", std::process::id());
                    return Some(PortLease { path, port });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lease_owner_dead(&path) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    break;
                }
                Err(_) => break,
            }
        }
    }
    None
}

/// Whether the lease file's recorded owner is provably gone. Errs on the
/// side of *live*: an unreadable or half-written lease is trusted (deleting
/// a just-created lease before its owner writes the pid would steal a live
/// port), and non-Unix platforms never reclaim (leases release via `Drop`).
fn lease_owner_dead(path: &Path) -> bool {
    let Some(pid) = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return false;
    };
    if pid == std::process::id() {
        return false;
    }
    #[cfg(unix)]
    {
        // `kill -0` probes liveness without signaling.
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Convenience: lease keyed by record id, mirroring the old in-memory map
/// shape the TUI keeps (`record_id → lease`).
pub type PortLeases = HashMap<String, PortLease>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_messages_round_trip_as_ndjson() {
        let msg = BridgeMsg::Permission {
            id: 7,
            handle: "abc123".into(),
            title: "WRITE src/x.rs".into(),
            diff: Some(WireDiff {
                path: "src/x.rs".into(),
                old: "a".into(),
                new: "b".into(),
            }),
            options: vec![WireOption {
                label: "allow".into(),
                outcome: "allow_once".into(),
            }],
        };
        let line = serde_json::to_string(&msg).expect("serialize");
        assert!(!line.contains('\n'), "NDJSON: one line per message");
        match serde_json::from_str::<BridgeMsg>(&line).expect("parse") {
            BridgeMsg::Permission { id, options, .. } => {
                assert_eq!(id, 7);
                assert_eq!(options[0].outcome, "allow_once");
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let resolve = TuiMsg::Resolve {
            id: 7,
            outcome: outcome_str(outcome_from_str("allow_always")).to_string(),
        };
        let line = serde_json::to_string(&resolve).expect("serialize");
        assert!(line.contains("allow_always"));
    }

    #[test]
    fn unknown_wire_outcome_fails_closed_to_deny() {
        assert_eq!(outcome_from_str("nonsense"), PermissionOutcome::Deny);
        assert_eq!(outcome_from_str("allow_once"), PermissionOutcome::AllowOnce);
    }

    #[test]
    fn branch_tag_sanitizes_for_git_ref_names() {
        assert_eq!(branch_tag("claude-acp"), "claude-acp");
        assert_eq!(branch_tag("my agent/v2"), "my-agent-v2");
        assert_eq!(branch_tag("gpt_4.1"), "gpt_4.1");
    }

    #[test]
    fn truncate_utf8_never_splits_a_character() {
        // '界' is 3 bytes; a cap landing mid-character must back off to the
        // previous boundary instead of panicking.
        let mut s = "ab界界".to_string(); // bytes: a(1) b(1) 界(3) 界(3)
        truncate_utf8(&mut s, 4);
        assert_eq!(s, "ab");

        let mut exact = "ab界界".to_string();
        truncate_utf8(&mut exact, 5);
        assert_eq!(exact, "ab界");

        let mut short = "ab".to_string();
        truncate_utf8(&mut short, 5);
        assert_eq!(short, "ab", "under the cap is untouched");

        let mut all_wide = "界".to_string();
        truncate_utf8(&mut all_wide, 2);
        assert_eq!(all_wide, "", "backs off to empty rather than panicking");
    }

    #[test]
    fn record16_strips_hyphens_and_caps_length() {
        assert_eq!(record16("abcd-efgh-ijkl-mnop-qrst"), "abcdefghijklmnop");
        assert_eq!(record16("ab-cd"), "abcd");
    }

    #[test]
    fn reserve_port_takes_lowest_free_and_releases_on_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = reserve_port(dir.path(), (3100, 3101)).expect("first lease");
        assert_eq!(a.port(), 3100);
        let b = reserve_port(dir.path(), (3100, 3101)).expect("second lease");
        assert_eq!(b.port(), 3101);
        assert!(
            reserve_port(dir.path(), (3100, 3101)).is_none(),
            "pool exhausted"
        );
        drop(a);
        let again = reserve_port(dir.path(), (3100, 3101)).expect("released port re-leased");
        assert_eq!(again.port(), 3100);
    }

    #[test]
    fn live_and_unreadable_leases_are_not_stolen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ports = dir.path().join(".bitrouter").join("ports");
        std::fs::create_dir_all(&ports).expect("mkdir");
        // Our own pid: live by definition.
        std::fs::write(ports.join("3100"), std::process::id().to_string()).expect("write");
        // Half-written lease (no pid yet): trusted, not reclaimed.
        std::fs::write(ports.join("3101"), "").expect("write");
        assert!(reserve_port(dir.path(), (3100, 3101)).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn dead_owner_lease_is_reclaimed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ports = dir.path().join(".bitrouter").join("ports");
        std::fs::create_dir_all(&ports).expect("mkdir");
        // A child that has already been reaped: its pid is provably dead
        // (modulo pid reuse in the microseconds between wait and probe).
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        let dead_pid = child.id();
        child.wait().expect("wait");
        std::fs::write(ports.join("3100"), dead_pid.to_string()).expect("write");
        let lease = reserve_port(dir.path(), (3100, 3100)).expect("stale lease reclaimed");
        assert_eq!(lease.port(), 3100);
    }
}
