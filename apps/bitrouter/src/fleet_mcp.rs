//! The fleet MCP bridge — `bitrouter mcp serve --backend fleet` (TUI_SPEC §4).
//!
//! A **stdio** MCP server the orchestrating harness launches as a subprocess;
//! its tools spawn and manage worktree-isolated ACP subagents. Stdio, not
//! HTTP, by design: these tools *mutate* (spawn processes, write your repo),
//! so they inherit the orchestrator's process identity instead of riding an
//! unauthenticated HTTP→local path (TUI_SPEC §15-Q2).
//!
//! The internal lifecycle is Task-shaped (MCP Tasks vocabulary — `working /
//! completed / failed`), but no shipping harness consumes the Tasks extension
//! yet, so every tool runs **blocking-with-summary**: `spawn_subagent` and
//! `prompt_subagent` return when the turn ends, carrying the reply, the typed
//! stop reason, and the worktree diff stat.
//!
//! **Writes are human-gated by default** (TUI_SPEC §5/§7): `apply_subagent`
//! and `merge_subagent` integrate a subagent's work into the base repository
//! and therefore refuse unless the human started the bridge with
//! `--allow-writes` — an explicit autonomy grant. Subagent permission
//! requests are auto-resolved by risk: reversible + in-worktree allows,
//! everything else denies (logged, never silent).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use bitrouter_sdk::acp::ConfigAcpRoutingTable;
use bitrouter_sdk::config::WorktreesConfig;
use bitrouter_substrate::engine::{LaunchOptions, Session};
use bitrouter_substrate::translate::{PermissionOutcome, SessionUpdateKind, select_option};
use futures::StreamExt;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::result_contract::ResultContract;
use crate::risk::Risk;

/// Reply text beyond this is truncated in tool summaries (the orchestrator
/// can `subagent_diff` for the work itself).
const MAX_REPLY_BYTES: usize = 32 * 1024;
/// Diff text beyond this is truncated with a note.
const MAX_DIFF_BYTES: usize = 64 * 1024;

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 character —
/// a raw `String::truncate` at a fixed byte offset panics mid-character
/// (agent replies and diffs routinely carry multibyte text).
fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// One managed subagent: the live session plus its integration metadata.
struct Subagent {
    session: Arc<Session>,
    agent_id: String,
    worktree: Option<PathBuf>,
    branch: Option<String>,
    /// The base repo `HEAD` commit at spawn — the diff/merge base.
    base_ref: String,
    port: Option<u16>,
    /// Task-shaped state: `working` while a turn is in flight, then
    /// `completed`/`failed` (MCP Tasks vocabulary, adopted internally now so
    /// the wire protocol is a capability flag later, not a rewrite).
    state: &'static str,
}

/// The fleet MCP server: tools over a registry of worktree-isolated
/// ACP subagents.
#[derive(Clone)]
pub struct FleetMcp {
    inner: Arc<FleetInner>,
    tool_router: ToolRouter<FleetMcp>,
}

struct FleetInner {
    catalog: ConfigAcpRoutingTable,
    base_repo: PathBuf,
    worktrees: WorktreesConfig,
    /// Human-granted write autonomy (`--allow-writes`).
    allow_writes: bool,
    /// handle (record16) → subagent. Also serializes integration: `apply`/
    /// `merge` hold this lock, so branches integrate one at a time.
    agents: tokio::sync::Mutex<HashMap<String, Subagent>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SpawnArgs {
    /// ACP agent id: a bundled-catalog id (`claude-acp`, `codex-acp`,
    /// `gemini-cli`) or a configured `agents:` entry.
    pub agent: String,
    /// The task prompt. Phrase it with clear boundaries and an output
    /// contract; the subagent works in an isolated worktree.
    pub task: String,
    /// Isolate in a fresh git worktree + branch (default true — set false
    /// only for read-only investigation tasks).
    pub worktree: Option<bool>,
    /// Optional JSON Schema the subagent's final reply must satisfy; the
    /// summary then carries `result`/`schema_ok` (one repair re-prompt).
    pub result_schema: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct HandleArgs {
    /// Subagent handle, as returned by `spawn_subagent`.
    pub handle: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PromptArgs {
    /// Subagent handle, as returned by `spawn_subagent`.
    pub handle: String,
    /// The follow-up prompt (e.g. review feedback to address).
    pub text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StatusArgs {
    /// Subagent handle; omit for a whole-fleet snapshot.
    pub handle: Option<String>,
}

#[tool_router]
impl FleetMcp {
    pub fn new(
        catalog: ConfigAcpRoutingTable,
        base_repo: PathBuf,
        worktrees: WorktreesConfig,
        allow_writes: bool,
    ) -> Self {
        Self {
            inner: Arc::new(FleetInner {
                catalog,
                base_repo,
                worktrees,
                allow_writes,
                agents: tokio::sync::Mutex::new(HashMap::new()),
            }),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Spawn a worktree-isolated ACP subagent, send it the task, and block until \
                       its turn ends. Returns a summary: handle, stop_reason, reply, diff stat \
                       (and result/schema_ok under result_schema). Subagents don't spawn \
                       subagents — keep delegation depth 1."
    )]
    async fn spawn_subagent(
        &self,
        Parameters(args): Parameters<SpawnArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.do_spawn(args).await {
            Ok(summary) => Ok(tool_json(&summary)),
            Err(e) => Ok(tool_error(&e)),
        }
    }

    #[tool(
        description = "Send a follow-up prompt to a running subagent and block until the turn \
                       ends. Same summary shape as spawn_subagent."
    )]
    async fn prompt_subagent(
        &self,
        Parameters(args): Parameters<PromptArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.do_prompt(args).await {
            Ok(summary) => Ok(tool_json(&summary)),
            Err(e) => Ok(tool_error(&e)),
        }
    }

    #[tool(
        description = "Fleet snapshot (or one subagent with handle): agent, state, worktree, \
                       branch, diff stat."
    )]
    async fn subagent_status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.do_status(args.handle.as_deref()).await {
            Ok(v) => Ok(tool_json(&v)),
            Err(e) => Ok(tool_error(&e)),
        }
    }

    #[tool(
        description = "The subagent's full diff against its spawn base (committed + uncommitted \
                       work in its worktree)."
    )]
    async fn subagent_diff(
        &self,
        Parameters(args): Parameters<HandleArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.do_diff(&args.handle).await {
            Ok(text) => Ok(CallToolResult::success(vec![ContentBlock::text(text)])),
            Err(e) => Ok(tool_error(&e)),
        }
    }

    #[tool(
        description = "Apply the subagent's diff onto the base repository working tree, \
                       UNCOMMITTED (the human writes the commit). Human-gated: requires the \
                       bridge to have been started with --allow-writes."
    )]
    async fn apply_subagent(
        &self,
        Parameters(args): Parameters<HandleArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.do_apply(&args.handle).await {
            Ok(v) => Ok(tool_json(&v)),
            Err(e) => Ok(tool_error(&e)),
        }
    }

    #[tool(
        description = "Merge the subagent's branch into the base repository, keeping history. \
                       Requires the subagent to have committed its work (clean worktree). \
                       Serialized: one integration at a time. Human-gated: requires \
                       --allow-writes."
    )]
    async fn merge_subagent(
        &self,
        Parameters(args): Parameters<HandleArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.do_merge(&args.handle).await {
            Ok(v) => Ok(tool_json(&v)),
            Err(e) => Ok(tool_error(&e)),
        }
    }

    #[tool(
        description = "Shut the subagent down. Its worktree is RETAINED (cleanup is gated on \
                       merged-or-discarded, never automatic)."
    )]
    async fn close_subagent(
        &self,
        Parameters(args): Parameters<HandleArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.do_close(&args.handle).await {
            Ok(v) => Ok(tool_json(&v)),
            Err(e) => Ok(tool_error(&e)),
        }
    }
}

impl FleetMcp {
    async fn do_spawn(&self, args: SpawnArgs) -> Result<serde_json::Value> {
        let inner = &self.inner;
        let isolate = args.worktree.unwrap_or(true);
        let contract = args
            .result_schema
            .as_ref()
            .map(|schema| ResultContract::from_flag(&schema.to_string()))
            .transpose()?;

        let port = {
            let agents = inner.agents.lock().await;
            let used: Vec<u16> = agents.values().filter_map(|a| a.port).collect();
            (inner.worktrees.ports.from..=inner.worktrees.ports.to).find(|p| !used.contains(p))
        };
        let tag = branch_tag(&args.agent);
        let options = LaunchOptions {
            worktree: isolate.then(|| bitrouter_substrate::worktree::WorktreeSpec {
                name: format!("{tag}-{{record16}}"),
                branch: Some(format!("bitrouter/{tag}-{{record16}}")),
                remove_on_shutdown: false,
            }),
            // Config-declared bootstrap: the human authored it and granted
            // this bridge by wiring it into the orchestrator — that is the
            // first-use approval for this surface.
            worktree_bootstrap: isolate.then(|| inner.worktrees.bootstrap.clone()).flatten(),
            env: port
                .map(|p| vec![("PORT".to_string(), p.to_string())])
                .unwrap_or_default(),
            ..Default::default()
        };
        let base_ref = git_stdout(&inner.base_repo, &["rev-parse", "HEAD"])
            .await
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        let session = Session::launch(
            &inner.catalog,
            &args.agent,
            inner.base_repo.clone(),
            options,
        )
        .await
        .with_context(|| format!("launching acp subagent '{}'", args.agent))?;
        let record_id = session.state().record_id.clone();
        let handle: String = record_id.chars().filter(|c| *c != '-').take(16).collect();
        let worktree = session.worktree_path().map(PathBuf::from);
        let branch = worktree
            .is_some()
            .then(|| format!("bitrouter/{tag}-{handle}"));
        let session = Arc::new(session);

        // Auto-policy: reversible + in-worktree allows; everything else
        // denies. Logged, never silent — there is no human in this loop.
        spawn_auto_policy(&session, inner.base_repo.clone(), handle.clone());

        {
            let mut agents = inner.agents.lock().await;
            agents.insert(
                handle.clone(),
                Subagent {
                    session: Arc::clone(&session),
                    agent_id: args.agent.clone(),
                    worktree: worktree.clone(),
                    branch: branch.clone(),
                    base_ref: base_ref.clone(),
                    port,
                    state: "working",
                },
            );
        }

        let summary = self
            .run_blocking_turn(&handle, session, &args.task, contract)
            .await?;
        Ok(summary)
    }

    async fn do_prompt(&self, args: PromptArgs) -> Result<serde_json::Value> {
        let session = {
            let mut agents = self.inner.agents.lock().await;
            let sub = agents
                .get_mut(&args.handle)
                .with_context(|| format!("no subagent with handle '{}'", args.handle))?;
            sub.state = "working";
            Arc::clone(&sub.session)
        };
        self.run_blocking_turn(&args.handle, session, &args.text, None)
            .await
    }

    /// Drive one blocking turn (with the optional result contract's repair
    /// loop) and assemble the Task-shaped summary.
    async fn run_blocking_turn(
        &self,
        handle: &str,
        session: Arc<Session>,
        text: &str,
        contract: Option<ResultContract>,
    ) -> Result<serde_json::Value> {
        let task = match &contract {
            Some(c) => format!("{text}{}", c.instruction()),
            None => text.to_string(),
        };
        let turn = collect_turn(&session, &task).await;
        let (response, reply) = match turn {
            Ok(v) => v,
            Err(e) => {
                self.set_state(handle, "failed").await;
                return Err(e);
            }
        };
        let (response, result, schema_ok) = match &contract {
            None => (response, None, None),
            Some(c) => match c.check(&reply) {
                Ok(v) => (response, Some(v), Some(true)),
                Err(problem) => {
                    // One repair re-prompt, then never block the orchestrator.
                    let (response, reply) =
                        collect_turn(&session, &c.repair_prompt(&problem)).await?;
                    match c.check(&reply) {
                        Ok(v) => (response, Some(v), Some(true)),
                        Err(_) => (response, Some(serde_json::Value::Null), Some(false)),
                    }
                }
            },
        };
        self.set_state(handle, "completed").await;

        let (agent_id, worktree, branch, base_ref, port) = self.meta(handle).await?;
        let diff_stat = match &worktree {
            Some(wt) => diff_stat(wt, &base_ref).await,
            None => None,
        };
        let mut reply = reply;
        if reply.len() > MAX_REPLY_BYTES {
            truncate_utf8(&mut reply, MAX_REPLY_BYTES);
            reply.push_str("\n… (truncated)");
        }
        let mut summary = serde_json::json!({
            "handle": handle,
            "agent": agent_id,
            "state": "completed",
            "stop_reason": response.stop_reason,
            "reply": reply,
            "worktree": worktree,
            "branch": branch,
            "port": port,
            "diff_stat": diff_stat,
        });
        if let Some(r) = result {
            summary["result"] = r;
            summary["schema_ok"] = serde_json::json!(schema_ok);
        }
        Ok(summary)
    }

    async fn set_state(&self, handle: &str, state: &'static str) {
        if let Some(sub) = self.inner.agents.lock().await.get_mut(handle) {
            sub.state = state;
        }
    }

    /// A subagent's integration metadata, cloned out of the registry.
    async fn meta(
        &self,
        handle: &str,
    ) -> Result<(String, Option<PathBuf>, Option<String>, String, Option<u16>)> {
        let agents = self.inner.agents.lock().await;
        let sub = agents
            .get(handle)
            .with_context(|| format!("no subagent with handle '{handle}'"))?;
        Ok((
            sub.agent_id.clone(),
            sub.worktree.clone(),
            sub.branch.clone(),
            sub.base_ref.clone(),
            sub.port,
        ))
    }

    async fn do_status(&self, handle: Option<&str>) -> Result<serde_json::Value> {
        let agents = self.inner.agents.lock().await;
        match handle {
            Some(h) => {
                let sub = agents
                    .get(h)
                    .with_context(|| format!("no subagent with handle '{h}'"))?;
                Ok(snapshot(h, sub).await)
            }
            None => {
                let mut fleet = Vec::new();
                for (h, sub) in agents.iter() {
                    fleet.push(snapshot(h, sub).await);
                }
                Ok(serde_json::json!({ "fleet": fleet }))
            }
        }
    }

    async fn do_diff(&self, handle: &str) -> Result<String> {
        let (_, worktree, _, base_ref, _) = self.meta(handle).await?;
        let wt = worktree.context("subagent has no worktree (spawned with worktree=false)")?;
        let mut diff = git_stdout(&wt, &["diff", &base_ref]).await?;
        let untracked = git_stdout(&wt, &["ls-files", "--others", "--exclude-standard"]).await?;
        if !untracked.trim().is_empty() {
            diff.push_str("\n# untracked files:\n");
            diff.push_str(&untracked);
        }
        if diff.len() > MAX_DIFF_BYTES {
            truncate_utf8(&mut diff, MAX_DIFF_BYTES);
            diff.push_str("\n… (truncated)");
        }
        if diff.trim().is_empty() {
            diff = "(no changes vs the spawn base)".to_string();
        }
        Ok(diff)
    }

    /// The human-gate on writes: `apply`/`merge` integrate into the base repo
    /// and bypass review, so they refuse without the explicit grant.
    fn require_write_grant(&self, verb: &str) -> Result<()> {
        if self.inner.allow_writes {
            return Ok(());
        }
        anyhow::bail!(
            "{verb} is human-gated by default: it writes to the base repository. Ask the \
             human to integrate from the review queue (`bitrouter tui`), or to restart \
             this bridge with `bitrouter mcp serve --backend fleet --allow-writes` to \
             grant write autonomy."
        )
    }

    async fn do_apply(&self, handle: &str) -> Result<serde_json::Value> {
        self.require_write_grant("apply_subagent")?;
        // Holding the registry lock serializes integration (merge-queue
        // semantics: one branch lands at a time).
        let agents = self.inner.agents.lock().await;
        let sub = agents
            .get(handle)
            .with_context(|| format!("no subagent with handle '{handle}'"))?;
        let wt = sub
            .worktree
            .as_ref()
            .context("subagent has no worktree to apply from")?;
        let patch = git_stdout(wt, &["diff", "--binary", &sub.base_ref]).await?;
        if patch.trim().is_empty() {
            anyhow::bail!("nothing to apply: the subagent's diff vs its spawn base is empty");
        }
        git_apply(&self.inner.base_repo, &patch)
            .await
            .context("applying the subagent's diff onto the base working tree")?;
        Ok(serde_json::json!({
            "handle": handle,
            "applied": true,
            "note": "changes are in the base working tree, uncommitted — the human writes the commit",
        }))
    }

    async fn do_merge(&self, handle: &str) -> Result<serde_json::Value> {
        self.require_write_grant("merge_subagent")?;
        let agents = self.inner.agents.lock().await;
        let sub = agents
            .get(handle)
            .with_context(|| format!("no subagent with handle '{handle}'"))?;
        let wt = sub
            .worktree
            .as_ref()
            .context("subagent has no worktree to merge")?;
        let branch = sub.branch.as_ref().context("subagent has no branch")?;
        let dirty = git_stdout(wt, &["status", "--porcelain"]).await?;
        if !dirty.trim().is_empty() {
            anyhow::bail!(
                "the subagent's worktree has uncommitted changes — have it commit its work \
                 (prompt_subagent), or use apply_subagent to stage the diff uncommitted"
            );
        }
        let msg = format!("merge {branch}");
        git_ok(
            &self.inner.base_repo,
            &["merge", "--no-ff", "-m", &msg, branch],
        )
        .await
        .context("merging the subagent's branch (resolve conflicts in the base repo)")?;
        Ok(serde_json::json!({
            "handle": handle,
            "merged": branch,
        }))
    }

    async fn do_close(&self, handle: &str) -> Result<serde_json::Value> {
        // Hold the registry lock across the sole-owner check: `do_prompt`
        // clones the session `Arc` under this same lock, so nothing can grab
        // a clone between the check and the removal.
        let mut agents = self.inner.agents.lock().await;
        let sub = agents
            .remove(handle)
            .with_context(|| format!("no subagent with handle '{handle}'"))?;
        let Subagent {
            session,
            agent_id,
            worktree,
            branch,
            base_ref,
            port,
            state,
        } = sub;
        let only = match Arc::try_unwrap(session) {
            Ok(only) => only,
            Err(session) => {
                // A turn is in flight — put the entry back; removing it here
                // would orphan the child process and its worktree lease.
                agents.insert(
                    handle.to_string(),
                    Subagent {
                        session,
                        agent_id,
                        worktree,
                        branch,
                        base_ref,
                        port,
                        state,
                    },
                );
                anyhow::bail!(
                    "subagent '{handle}' still has a turn in flight — wait for it to finish"
                );
            }
        };
        drop(agents);
        only.shutdown()
            .await
            .context("shutting down the subagent session")?;
        Ok(serde_json::json!({
            "handle": handle,
            "closed": true,
            "worktree_retained": worktree,
        }))
    }
}

/// Consume a subagent's permission stream with the risk auto-policy:
/// reversible + in-worktree ⇒ allow-once; everything else ⇒ deny. Every
/// decision is logged to stderr (never silent).
fn spawn_auto_policy(session: &Arc<Session>, workroot: PathBuf, handle: String) {
    let mut perms = session.permissions();
    tokio::spawn(async move {
        while let Some(pending) = perms.next().await {
            let title = pending
                .tool_call
                .fields
                .title
                .clone()
                .unwrap_or_else(|| "(unnamed)".to_string());
            match crate::risk::classify(&pending.tool_call.fields, &workroot) {
                Risk::Low => {
                    tracing::info!(subagent = %handle, tool = %title, "auto-allowed (low risk)");
                    let selected = select_option(PermissionOutcome::AllowOnce, &pending.options);
                    pending.resolve(selected);
                }
                Risk::High => {
                    tracing::warn!(subagent = %handle, tool = %title, "denied (high risk, no human in the loop)");
                    drop(pending); // dropping resolves as the reject option
                }
            }
        }
    });
}

/// Drive one prompt turn and collect the reply's message text.
async fn collect_turn(
    session: &Session,
    text: &str,
) -> Result<(agent_client_protocol::schema::v1::PromptResponse, String)> {
    let mut updates = session.updates();
    let mut reply = String::new();
    let response = {
        let prompt_future = session.prompt(text);
        tokio::pin!(prompt_future);
        loop {
            tokio::select! {
                biased;
                result = &mut prompt_future => {
                    let response = result.context("subagent prompt failed")?;
                    // Non-blocking drain of already-buffered updates.
                    loop {
                        let maybe = tokio::select! {
                            biased;
                            v = updates.next() => v,
                            _ = std::future::ready(()) => None,
                        };
                        match maybe {
                            Some(SessionUpdateKind::MessageChunk { text, .. }) => reply.push_str(&text),
                            Some(_) => {}
                            None => break,
                        }
                    }
                    break response;
                }
                maybe_update = updates.next() => {
                    if let Some(SessionUpdateKind::MessageChunk { text, .. }) = maybe_update {
                        reply.push_str(&text);
                    }
                }
            }
        }
    };
    Ok((response, reply))
}

/// One subagent's status snapshot.
async fn snapshot(handle: &str, sub: &Subagent) -> serde_json::Value {
    serde_json::json!({
        "handle": handle,
        "agent": sub.agent_id,
        "state": sub.state,
        "worktree": sub.worktree,
        "branch": sub.branch,
        "port": sub.port,
        "diff_stat": match &sub.worktree {
            Some(wt) => diff_stat(wt, &sub.base_ref).await,
            None => None,
        },
    })
}

/// `+adds/-dels/files` over the worktree vs its spawn base (tracked changes).
pub(crate) async fn diff_stat(
    worktree: &std::path::Path,
    base_ref: &str,
) -> Option<serde_json::Value> {
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

/// Branch-safe agent tag: keep `[A-Za-z0-9._]`, everything else becomes `-`.
/// Shared with the TUI's fleet spawns.
pub(crate) fn branch_tag(agent_id: &str) -> String {
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

/// Run git in `dir`, capturing stdout; errors carry stderr.
pub(crate) async fn git_stdout(dir: &std::path::Path, args: &[&str]) -> Result<String> {
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
pub(crate) async fn git_ok(dir: &std::path::Path, args: &[&str]) -> Result<()> {
    git_stdout(dir, args).await.map(|_| ())
}

/// `git apply` the patch text onto `dir`'s working tree (3-way for context
/// drift; fails clean on conflicts).
pub(crate) async fn git_apply(dir: &std::path::Path, patch: &str) -> Result<()> {
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

/// Serialize `value` into a successful tool result.
fn tool_json(value: &serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.to_string())])
}

/// Surface an operation failure as a tool error (the orchestrator sees the
/// message and can adjust).
fn tool_error(e: &anyhow::Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))])
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FleetMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "BitRouter fleet bridge: spawn and manage worktree-isolated ACP subagents. \
             spawn_subagent blocks and returns a summary; review with subagent_diff; \
             apply/merge are human-gated unless the bridge was granted --allow-writes."
                .to_string(),
        )
    }
}

/// Run the fleet bridge over stdio until the orchestrator disconnects.
pub async fn serve_stdio(
    catalog: ConfigAcpRoutingTable,
    base_repo: PathBuf,
    worktrees: WorktreesConfig,
    allow_writes: bool,
) -> Result<()> {
    use rmcp::ServiceExt;
    let server = FleetMcp::new(catalog, base_repo, worktrees, allow_writes);
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_verbs_are_gated_without_the_grant() {
        let fleet = FleetMcp::new(
            ConfigAcpRoutingTable::from_configs(std::iter::empty()).expect("empty catalog"),
            PathBuf::from("/tmp"),
            WorktreesConfig::default(),
            false,
        );
        let err = fleet
            .require_write_grant("merge_subagent")
            .expect_err("writes must be human-gated by default");
        let msg = format!("{err:#}");
        assert!(msg.contains("--allow-writes"), "actionable: {msg}");
        assert!(msg.contains("human-gated"), "names the policy: {msg}");

        let granted = FleetMcp::new(
            ConfigAcpRoutingTable::from_configs(std::iter::empty()).expect("empty catalog"),
            PathBuf::from("/tmp"),
            WorktreesConfig::default(),
            true,
        );
        assert!(granted.require_write_grant("merge_subagent").is_ok());
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
}

#[cfg(all(test, unix))]
mod e2e_tests {
    use super::*;

    /// Bash ACP stub that ACTS like a coding subagent: on `session/new` it
    /// `cd`s into the relayed cwd (the worktree); on the first prompt it
    /// writes a file and commits it on the session branch, then answers.
    const WORKER_STUB: &str = r#"
        while read line; do
          id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
          case "$line" in
            *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
            *session/new*)  wd=$(echo "$line" | sed -n 's/.*"cwd":"\([^"]*\)".*/\1/p'); cd "$wd" 2>/dev/null
                            printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
            *session/prompt*)
              echo made > made.txt
              git add made.txt >/dev/null 2>&1
              git -c user.email=t@t -c user.name=t commit -qm work >/dev/null 2>&1
              printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}}}\n'
              printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
          esac
        done
    "#;

    fn worker_catalog() -> ConfigAcpRoutingTable {
        let cfg = bitrouter_sdk::acp::AcpAgentConfig {
            name: "stub".to_string(),
            transport: bitrouter_sdk::acp::AcpTransport::Stdio {
                command: "bash".to_string(),
                args: vec!["-c".to_string(), WORKER_STUB.to_string()],
                env: HashMap::new(),
            },
        };
        ConfigAcpRoutingTable::from_configs([("stub".to_string(), cfg)]).expect("catalog")
    }

    fn init_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().expect("tempdir");
        for a in [
            &["init", "-q"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
        ] {
            std::process::Command::new("git")
                .current_dir(d.path())
                .args(a)
                .status()
                .expect("git");
        }
        std::fs::write(d.path().join("f"), "x").expect("write");
        std::process::Command::new("git")
            .current_dir(d.path())
            .args(["add", "."])
            .status()
            .expect("git");
        std::process::Command::new("git")
            .current_dir(d.path())
            .args(["commit", "-qm", "init"])
            .status()
            .expect("git");
        d
    }

    #[tokio::test]
    async fn spawn_review_merge_close_roundtrip() {
        let repo = init_repo();
        let fleet = FleetMcp::new(
            worker_catalog(),
            repo.path().to_path_buf(),
            WorktreesConfig::default(),
            true, // write autonomy granted for this test
        );

        // ── spawn (blocking-with-summary) ──
        let summary = fleet
            .do_spawn(SpawnArgs {
                agent: "stub".into(),
                task: "write made.txt".into(),
                worktree: None, // default: isolated
                result_schema: None,
            })
            .await
            .expect("spawn");
        assert_eq!(summary["state"], "completed");
        assert_eq!(summary["stop_reason"], "end_turn");
        assert_eq!(summary["agent"], "stub");
        assert!(
            summary["reply"]
                .as_str()
                .is_some_and(|r| r.contains("done"))
        );
        let handle = summary["handle"].as_str().expect("handle").to_string();
        let branch = summary["branch"].as_str().expect("branch").to_string();
        assert!(
            branch.starts_with("bitrouter/stub-"),
            "branch naming: {branch}"
        );
        let stat = &summary["diff_stat"];
        assert_eq!(stat["files"], 1, "one file changed: {summary}");
        assert_eq!(stat["adds"], 1);

        // ── status + diff ──
        let status = fleet.do_status(Some(&handle)).await.expect("status");
        assert_eq!(status["state"], "completed");
        let diff = fleet.do_diff(&handle).await.expect("diff");
        assert!(
            diff.contains("made.txt") && diff.contains("+made"),
            "{diff}"
        );

        // ── merge (serialized, keeps history) ──
        let merged = fleet.do_merge(&handle).await.expect("merge");
        assert_eq!(merged["merged"], branch);
        assert!(
            repo.path().join("made.txt").exists(),
            "subagent work landed in the base repo"
        );

        // ── close: worktree retained ──
        let closed = fleet.do_close(&handle).await.expect("close");
        assert_eq!(closed["closed"], true);
        let wt = closed["worktree_retained"].as_str().expect("worktree");
        assert!(
            std::path::Path::new(wt).exists(),
            "worktree retained after close (cleanup gated on merged-or-discarded)"
        );
        assert!(
            fleet.do_status(Some(&handle)).await.is_err(),
            "closed handle no longer resolves"
        );
    }

    #[tokio::test]
    async fn apply_stages_diff_uncommitted() {
        let repo = init_repo();
        let fleet = FleetMcp::new(
            worker_catalog(),
            repo.path().to_path_buf(),
            WorktreesConfig::default(),
            true,
        );
        let summary = fleet
            .do_spawn(SpawnArgs {
                agent: "stub".into(),
                task: "write made.txt".into(),
                worktree: None,
                result_schema: None,
            })
            .await
            .expect("spawn");
        let handle = summary["handle"].as_str().expect("handle").to_string();

        let applied = fleet.do_apply(&handle).await.expect("apply");
        assert_eq!(applied["applied"], true);
        assert!(repo.path().join("made.txt").exists(), "diff applied");
        // Uncommitted: the human writes the commit.
        let porcelain = std::process::Command::new("git")
            .current_dir(repo.path())
            .args(["status", "--porcelain"])
            .output()
            .expect("git status");
        assert!(
            String::from_utf8_lossy(&porcelain.stdout).contains("made.txt"),
            "applied changes are uncommitted in the base working tree"
        );
        fleet.do_close(&handle).await.expect("close");
    }
}
