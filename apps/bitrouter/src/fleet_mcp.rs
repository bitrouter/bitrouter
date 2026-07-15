//! The substrate-backed adapter for the fleet capability — the app side of
//! `bitrouter mcp serve --backend fleet` (TUI_SPEC §4).
//!
//! The MCP handler and every tool schema live in `bitrouter-mcp`; this module
//! implements that crate's [`Fleet`](bitrouter_mcp::capabilities::fleet::Fleet)
//! port against `bitrouter_substrate`. All substrate-coupled behavior stays
//! here so the crate never depends on the substrate.
//!
//! The orchestrator profile is **stdio-only** by design: these tools *mutate*
//! (spawn processes, write your repo), so they inherit the orchestrator's
//! process identity instead of riding an unauthenticated HTTP→local path
//! (TUI_SPEC §15-Q2).
//!
//! The internal lifecycle is Task-shaped (MCP Tasks vocabulary — `working /
//! completed / failed`), but no shipping harness consumes the Tasks extension
//! yet, so every tool runs **blocking-with-summary**: `spawn` and `prompt`
//! return when the turn ends, carrying the reply, the typed stop reason, and
//! the worktree diff stat.
//!
//! **Writes are human-gated by default** (TUI_SPEC §5/§7): `apply` and `merge`
//! integrate a subagent's work into the base repository and therefore refuse
//! unless the human started the bridge with `--allow-writes` — an explicit
//! autonomy grant. Subagent permission requests are auto-resolved by risk:
//! reversible + in-worktree allows; everything else escalates to the hosting
//! TUI's decision queue when this bridge runs under `bitrouter tui` (it
//! connects back over the fleet socket, mirroring its subagents into the
//! rail), and denies when headless (logged, never silent).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use bitrouter_mcp::capabilities::fleet::{Fleet, PromptArgs, SpawnArgs};
use bitrouter_mcp::error::ToolError;
use bitrouter_sdk::acp::ConfigAcpRoutingTable;
use bitrouter_sdk::config::WorktreesConfig;
use bitrouter_substrate::engine::{LaunchOptions, Session};
use bitrouter_substrate::translate::{PermissionOutcome, SessionUpdateKind, select_option};
use futures::StreamExt;

use crate::fleet::truncate_utf8;
use crate::result_contract::ResultContract;
use crate::risk::Risk;

/// Reply text beyond this is truncated in tool summaries (the orchestrator
/// can `subagent_diff` for the work itself).
const MAX_REPLY_BYTES: usize = 32 * 1024;
/// Diff text beyond this is truncated with a note.
const MAX_DIFF_BYTES: usize = 64 * 1024;
/// Max subagents one fleet bridge will run at once — the circuit-breaker
/// slice of TUI_SPEC §5. TUI_SPEC §13 sizes a healthy fleet at ~2–6
/// subagents; past this the orchestrator should integrate or close one
/// (`merge_subagent` / `close_subagent`) before spawning more, rather than
/// fan out unboundedly.
const MAX_CONCURRENT_SUBAGENTS: usize = 6;

/// One managed subagent: the live session plus its integration metadata.
struct Subagent {
    session: Arc<Session>,
    agent_id: String,
    worktree: Option<PathBuf>,
    branch: Option<String>,
    /// The base repo `HEAD` commit at spawn — the diff/merge base.
    base_ref: String,
    /// Cross-process `PORT` claim; dropping the lease frees the port.
    port: Option<crate::fleet::PortLease>,
    /// Task-shaped state: `working` while a turn is in flight, then
    /// `completed`/`failed` (MCP Tasks vocabulary, adopted internally now so
    /// the wire protocol is a capability flag later, not a rewrite).
    state: &'static str,
}

/// A freshly launched-and-registered subagent, handed back to `do_spawn` so it
/// can run the opening turn. The reservation has already been consumed and the
/// subagent inserted by the time this is returned.
struct Launched {
    handle: String,
    session: Arc<Session>,
    /// The opening task prompt (moved out of `SpawnArgs`).
    task: String,
    contract: Option<ResultContract>,
    /// Whether the bootstrap hook was skipped (surfaced in the spawn summary).
    bootstrap_skipped: bool,
}

/// The substrate-backed fleet: a registry of worktree-isolated ACP subagents.
/// Injected into `bitrouter-mcp`'s orchestrator profile as the `Fleet` port.
pub struct SubstrateFleet {
    inner: Arc<FleetInner>,
}

struct FleetInner {
    catalog: ConfigAcpRoutingTable,
    base_repo: PathBuf,
    worktrees: WorktreesConfig,
    /// Human-granted write autonomy (`--allow-writes`).
    allow_writes: bool,
    /// The live subagent registry plus in-flight spawn reservations, under one
    /// lock. Also serializes integration: `apply`/`merge` hold this lock, so
    /// branches integrate one at a time.
    registry: tokio::sync::Mutex<Registry>,
    /// Live link back to the hosting TUI over the fleet socket, when this
    /// bridge was launched under `bitrouter tui` (Unix): mirrors the fleet
    /// into the rail and routes gated permissions to the human's queue.
    #[cfg(unix)]
    link: Option<Arc<TuiLink>>,
}

/// The subagent map plus the count of in-flight spawns that have reserved a
/// slot but not yet inserted. Counting reservations against the cap — under the
/// same lock as the map — is what stops N concurrent `spawn_subagent` calls
/// from each passing the capacity check and overshooting
/// [`MAX_CONCURRENT_SUBAGENTS`]: the check and the reservation are one critical
/// section, and the reservation converts to a live entry (or is released) under
/// that same lock.
#[derive(Default)]
struct Registry {
    /// handle (record16) → subagent.
    agents: HashMap<String, Subagent>,
    /// Slots claimed by spawns still launching (a claim is released either when
    /// the subagent is inserted or when its launch fails).
    reserving: usize,
}

/// The bridge's end of the TUI fleet socket.
#[cfg(unix)]
struct TuiLink {
    writer: tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>,
    /// Standing policy from `TuiMsg::Hello` / `BootstrapApproved`: whether
    /// the human approved the worktree bootstrap hook.
    bootstrap_approved: std::sync::atomic::AtomicBool,
    /// In-flight permission requests awaiting the human, by bridge-local id.
    pending: tokio::sync::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<String>>>,
    next_id: std::sync::atomic::AtomicU64,
}

#[cfg(unix)]
impl TuiLink {
    /// Connect to the TUI's fleet socket and start the reader that routes
    /// `TuiMsg`s (policy updates, permission resolutions) back in.
    async fn connect(path: &str) -> Option<Arc<Self>> {
        let stream = match tokio::net::UnixStream::connect(path).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, path, "fleet TUI socket connect failed");
                return None;
            }
        };
        let (read, write) = stream.into_split();
        let link = Arc::new(TuiLink {
            writer: tokio::sync::Mutex::new(write),
            bootstrap_approved: std::sync::atomic::AtomicBool::new(false),
            pending: tokio::sync::Mutex::new(HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
        });
        let reader_link = Arc::clone(&link);
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<crate::fleet::TuiMsg>(&line) {
                    Ok(crate::fleet::TuiMsg::Hello { bootstrap_approved }) => {
                        reader_link
                            .bootstrap_approved
                            .store(bootstrap_approved, std::sync::atomic::Ordering::SeqCst);
                    }
                    Ok(crate::fleet::TuiMsg::BootstrapApproved) => {
                        reader_link
                            .bootstrap_approved
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    Ok(crate::fleet::TuiMsg::Resolve { id, outcome }) => {
                        if let Some(sender) = reader_link.pending.lock().await.remove(&id) {
                            let _ = sender.send(outcome);
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "unparseable fleet TUI message"),
                }
            }
            // Socket closed (TUI exited): every waiting permission denies.
            reader_link.pending.lock().await.clear();
        });
        Some(link)
    }

    /// Send one NDJSON message (best-effort).
    async fn send(&self, msg: &crate::fleet::BridgeMsg) {
        use tokio::io::AsyncWriteExt;
        if let Ok(mut line) = serde_json::to_string(msg) {
            line.push('\n');
            let _ = self.writer.lock().await.write_all(line.as_bytes()).await;
        }
    }

    /// Route a gated permission to the human's decision queue and await the
    /// resolution. `None` when the link died first (the caller denies).
    async fn request_permission(
        &self,
        handle: &str,
        pending: &bitrouter_substrate::up::PendingPermission,
    ) -> Option<bitrouter_substrate::translate::PermissionOutcome> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let title = pending
            .tool_call
            .fields
            .title
            .clone()
            .unwrap_or_else(|| "(unnamed)".to_string());
        self.send(&crate::fleet::BridgeMsg::Permission {
            id,
            handle: handle.to_string(),
            title,
            diff: crate::fleet::wire_diff(pending),
            options: crate::fleet::wire_options(pending),
        })
        .await;
        let outcome = receiver.await.ok()?;
        Some(crate::fleet::outcome_from_str(&outcome))
    }
}

impl SubstrateFleet {
    /// Build the fleet over `base_repo` and, on unix, connect back to the
    /// hosting TUI's fleet socket when one is advertised (`BITROUTER_FLEET_TUI_SOCK`).
    /// Linked, the fleet mirrors into the rail and gated permissions reach the
    /// human's decision queue instead of the headless deny.
    pub async fn connect(
        catalog: ConfigAcpRoutingTable,
        base_repo: PathBuf,
        worktrees: WorktreesConfig,
        allow_writes: bool,
    ) -> Self {
        #[cfg(unix)]
        let link = match std::env::var(crate::fleet::TUI_SOCK_ENV) {
            Ok(path) => TuiLink::connect(&path).await,
            Err(_) => None,
        };
        let inner = FleetInner {
            catalog,
            base_repo,
            worktrees,
            allow_writes,
            registry: tokio::sync::Mutex::new(Registry::default()),
            #[cfg(unix)]
            link,
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    async fn do_spawn(&self, args: SpawnArgs) -> Result<serde_json::Value> {
        let inner = &self.inner;
        // Circuit breaker: cap the live fleet so the orchestrator integrates
        // or closes before fanning out unboundedly (TUI_SPEC §5). Reserve a
        // slot atomically — the capacity check and the reservation are one
        // critical section — so N concurrent spawns can't all pass the check
        // and overshoot. The reservation is consumed at insert time, or
        // released here on a launch failure (never leaks a slot).
        {
            let mut reg = inner.registry.lock().await;
            if reg.agents.len() + reg.reserving >= MAX_CONCURRENT_SUBAGENTS {
                anyhow::bail!(
                    "fleet at capacity: {MAX_CONCURRENT_SUBAGENTS} subagents already running. \
                     Integrate or close one (merge_subagent / apply_subagent / close_subagent) \
                     before spawning more."
                );
            }
            reg.reserving += 1;
        }
        let launched = self.launch_and_register(args).await;
        let Launched {
            handle,
            session,
            task,
            contract,
            bootstrap_skipped,
        } = match launched {
            Ok(v) => v,
            Err(e) => {
                // Launch failed before the subagent was inserted — release the
                // slot we reserved. The port lease and half-built session drop
                // on the `?`-unwind inside `launch_and_register` (no leak).
                inner.registry.lock().await.reserving -= 1;
                return Err(e);
            }
        };

        let mut summary = self
            .run_blocking_turn(&handle, session, &task, contract)
            .await?;
        if bootstrap_skipped {
            summary["bootstrap"] = serde_json::json!(
                "skipped — the worktree bootstrap hook isn't approved in the TUI yet \
                 (the human approves it on their first spawn there)"
            );
        }
        Ok(summary)
    }

    /// Launch the subagent and register it, consuming the caller's reservation
    /// (`reserving -= 1`) in the same critical section as the insert so the
    /// live count never overshoots the cap. Any failure before the insert
    /// returns `Err` with the reservation untouched — `do_spawn` releases it —
    /// and the freshly-leased port / half-built session drop on the unwind.
    async fn launch_and_register(&self, args: SpawnArgs) -> Result<Launched> {
        let inner = &self.inner;
        let isolate = args.worktree.unwrap_or(true);
        let contract = args
            .result_schema
            .as_ref()
            .map(|schema| ResultContract::from_flag(&schema.to_string()))
            .transpose()?;

        // Lease-file pool: atomic across the TUI's fleet and every bridge
        // subprocess, and across two concurrent spawn_subagent calls (the
        // old registry scan raced between the scan and the insert).
        let port = crate::fleet::reserve_port(
            &inner.base_repo,
            (inner.worktrees.ports.from, inner.worktrees.ports.to),
        );
        let tag = crate::fleet::branch_tag(&args.agent);
        // The bootstrap hook executes shell on worktree creation. Linked to
        // a TUI, it runs only after the human's first-use approval there
        // (same discipline as TUI spawns); headless, the config author's
        // wiring of this bridge is the standing grant.
        let bootstrap_cmd = isolate.then(|| inner.worktrees.bootstrap.clone()).flatten();
        #[cfg(unix)]
        let bootstrap_gated = inner.link.as_ref().is_some_and(|l| {
            !l.bootstrap_approved
                .load(std::sync::atomic::Ordering::SeqCst)
        });
        #[cfg(not(unix))]
        let bootstrap_gated = false;
        let bootstrap_skipped = bootstrap_gated && bootstrap_cmd.is_some();
        let options = LaunchOptions {
            worktree: isolate.then(|| crate::fleet::worktree_spec(&tag)),
            worktree_bootstrap: if bootstrap_gated { None } else { bootstrap_cmd },
            env: crate::fleet::port_env(port.as_ref().map(|l| l.port())),
            ..Default::default()
        };
        let base_ref = crate::fleet::base_head(&inner.base_repo).await;

        let session = Session::launch(
            &inner.catalog,
            &args.agent,
            inner.base_repo.clone(),
            options,
        )
        .await
        .with_context(|| format!("launching acp subagent '{}'", args.agent))?;
        let record_id = session.state().record_id.clone();
        let handle = crate::fleet::record16(&record_id);
        let worktree = session.worktree_path().map(PathBuf::from);
        let branch = worktree
            .is_some()
            .then(|| format!("bitrouter/{tag}-{handle}"));
        let session = Arc::new(session);

        // Auto-policy: reversible + in-worktree allows; everything else
        // escalates to the linked TUI's decision queue, or denies headless.
        spawn_auto_policy(inner, &session, handle.clone());

        let port_num = port.as_ref().map(|l| l.port());
        {
            let mut reg = inner.registry.lock().await;
            // Convert the reservation into a live entry atomically: the count
            // moves from `reserving` to `agents` under one lock, so a concurrent
            // spawn never sees the slot double-counted or dropped.
            reg.reserving -= 1;
            reg.agents.insert(
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
        // Mirror the spawn into the hosting TUI's rail.
        #[cfg(unix)]
        if let Some(link) = &inner.link {
            link.send(&crate::fleet::BridgeMsg::Spawned {
                handle: handle.clone(),
                agent: args.agent.clone(),
                port: port_num,
            })
            .await;
        }
        #[cfg(not(unix))]
        let _ = port_num;

        Ok(Launched {
            handle,
            session,
            task: args.task,
            contract,
            bootstrap_skipped,
        })
    }

    async fn do_prompt(&self, args: PromptArgs) -> Result<serde_json::Value> {
        let session = {
            let mut reg = self.inner.registry.lock().await;
            let sub = reg
                .agents
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
            Some(wt) => crate::fleet::diff_stat(wt, &base_ref).await,
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
        if let Some(sub) = self.inner.registry.lock().await.agents.get_mut(handle) {
            sub.state = state;
        }
        #[cfg(unix)]
        if let Some(link) = &self.inner.link {
            link.send(&crate::fleet::BridgeMsg::State {
                handle: handle.to_string(),
                state: state.to_string(),
            })
            .await;
        }
    }

    /// A subagent's integration metadata, cloned out of the registry.
    async fn meta(
        &self,
        handle: &str,
    ) -> Result<(String, Option<PathBuf>, Option<String>, String, Option<u16>)> {
        let reg = self.inner.registry.lock().await;
        let sub = reg
            .agents
            .get(handle)
            .with_context(|| format!("no subagent with handle '{handle}'"))?;
        Ok((
            sub.agent_id.clone(),
            sub.worktree.clone(),
            sub.branch.clone(),
            sub.base_ref.clone(),
            sub.port.as_ref().map(|l| l.port()),
        ))
    }

    async fn do_status(&self, handle: Option<&str>) -> Result<serde_json::Value> {
        let reg = self.inner.registry.lock().await;
        match handle {
            Some(h) => {
                let sub = reg
                    .agents
                    .get(h)
                    .with_context(|| format!("no subagent with handle '{h}'"))?;
                Ok(snapshot(h, sub).await)
            }
            None => {
                let mut fleet = Vec::new();
                for (h, sub) in reg.agents.iter() {
                    fleet.push(snapshot(h, sub).await);
                }
                Ok(serde_json::json!({ "fleet": fleet }))
            }
        }
    }

    async fn do_diff(&self, handle: &str) -> Result<String> {
        let (_, worktree, _, base_ref, _) = self.meta(handle).await?;
        let wt = worktree.context("subagent has no worktree (spawned with worktree=false)")?;
        let mut diff = crate::fleet::git_stdout(&wt, &["diff", &base_ref]).await?;
        let untracked =
            crate::fleet::git_stdout(&wt, &["ls-files", "--others", "--exclude-standard"]).await?;
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
        let reg = self.inner.registry.lock().await;
        let sub = reg
            .agents
            .get(handle)
            .with_context(|| format!("no subagent with handle '{handle}'"))?;
        let wt = sub
            .worktree
            .as_ref()
            .context("subagent has no worktree to apply from")?;
        crate::fleet::apply_diff(&self.inner.base_repo, wt, &sub.base_ref)
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
        let reg = self.inner.registry.lock().await;
        let sub = reg
            .agents
            .get(handle)
            .with_context(|| format!("no subagent with handle '{handle}'"))?;
        let wt = sub
            .worktree
            .as_ref()
            .context("subagent has no worktree to merge")?;
        let branch = sub.branch.as_ref().context("subagent has no branch")?;
        crate::fleet::merge_branch(
            &self.inner.base_repo,
            wt,
            branch,
            "have it commit its work (prompt_subagent), or use apply_subagent to stage \
             the diff uncommitted",
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
        let mut reg = self.inner.registry.lock().await;
        let sub = reg
            .agents
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
                reg.agents.insert(
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
        drop(reg);
        only.shutdown()
            .await
            .context("shutting down the subagent session")?;
        #[cfg(unix)]
        if let Some(link) = &self.inner.link {
            link.send(&crate::fleet::BridgeMsg::Closed {
                handle: handle.to_string(),
            })
            .await;
        }
        Ok(serde_json::json!({
            "handle": handle,
            "closed": true,
            "worktree_retained": worktree,
        }))
    }
}

/// Map an adapter error into the crate's substrate-free carrier (`{e:#}`
/// keeps anyhow's context chain).
fn to_tool_error(e: anyhow::Error) -> ToolError {
    ToolError::new(format!("{e:#}"))
}

#[async_trait::async_trait]
impl Fleet for SubstrateFleet {
    async fn spawn(&self, args: SpawnArgs) -> Result<serde_json::Value, ToolError> {
        self.do_spawn(args).await.map_err(to_tool_error)
    }

    async fn prompt(&self, args: PromptArgs) -> Result<serde_json::Value, ToolError> {
        self.do_prompt(args).await.map_err(to_tool_error)
    }

    async fn status(&self, handle: Option<&str>) -> Result<serde_json::Value, ToolError> {
        self.do_status(handle).await.map_err(to_tool_error)
    }

    async fn diff(&self, handle: &str) -> Result<String, ToolError> {
        self.do_diff(handle).await.map_err(to_tool_error)
    }

    async fn apply(&self, handle: &str) -> Result<serde_json::Value, ToolError> {
        self.do_apply(handle).await.map_err(to_tool_error)
    }

    async fn merge(&self, handle: &str) -> Result<serde_json::Value, ToolError> {
        self.do_merge(handle).await.map_err(to_tool_error)
    }

    async fn close(&self, handle: &str) -> Result<serde_json::Value, ToolError> {
        self.do_close(handle).await.map_err(to_tool_error)
    }
}

/// Consume a subagent's permission stream with the risk auto-policy:
/// reversible + in-worktree ⇒ allow-once; everything else escalates to the
/// hosting TUI's decision queue when this bridge is linked (TUI_SPEC §5's
/// escalation home), and denies when headless. Every decision is logged to
/// stderr (never silent).
fn spawn_auto_policy(inner: &Arc<FleetInner>, session: &Arc<Session>, handle: String) {
    let mut perms = session.permissions();
    let workroot = inner.base_repo.clone();
    #[cfg(unix)]
    let link = inner.link.clone();
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
                    #[cfg(unix)]
                    if let Some(link) = &link {
                        match link.request_permission(&handle, &pending).await {
                            Some(outcome) => {
                                tracing::info!(
                                    subagent = %handle, tool = %title, outcome = ?outcome,
                                    "resolved by the human (TUI decision queue)"
                                );
                                let selected = select_option(outcome, &pending.options);
                                pending.resolve(selected);
                            }
                            None => {
                                tracing::warn!(
                                    subagent = %handle, tool = %title,
                                    "TUI link lost — denied (high risk)"
                                );
                                drop(pending);
                            }
                        }
                        continue;
                    }
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
        "port": sub.port.as_ref().map(|l| l.port()),
        "diff_stat": match &sub.worktree {
            Some(wt) => crate::fleet::diff_stat(wt, &sub.base_ref).await,
            None => None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_verbs_are_gated_without_the_grant() {
        let fleet = SubstrateFleet::connect(
            ConfigAcpRoutingTable::from_configs(std::iter::empty()).expect("empty catalog"),
            PathBuf::from("/tmp"),
            WorktreesConfig::default(),
            false,
        )
        .await;
        let err = fleet
            .require_write_grant("merge_subagent")
            .expect_err("writes must be human-gated by default");
        let msg = format!("{err:#}");
        assert!(msg.contains("--allow-writes"), "actionable: {msg}");
        assert!(msg.contains("human-gated"), "names the policy: {msg}");

        let granted = SubstrateFleet::connect(
            ConfigAcpRoutingTable::from_configs(std::iter::empty()).expect("empty catalog"),
            PathBuf::from("/tmp"),
            WorktreesConfig::default(),
            true,
        )
        .await;
        assert!(granted.require_write_grant("merge_subagent").is_ok());
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

    /// A minimal stub that just answers — no git writes. Used by the
    /// concurrency-cap test, which only needs the registry to fill up.
    const IDLE_STUB: &str = r#"
        while read line; do
          id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
          case "$line" in
            *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
            *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
            *session/prompt*)
              printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ok"}}}}\n'
              printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
          esac
        done
    "#;

    fn catalog_from(name: &str, script: &str) -> ConfigAcpRoutingTable {
        let cfg = bitrouter_sdk::acp::AcpAgentConfig {
            name: name.to_string(),
            transport: bitrouter_sdk::acp::AcpTransport::Stdio {
                command: "bash".to_string(),
                args: vec!["-c".to_string(), script.to_string()],
                env: HashMap::new(),
            },
        };
        ConfigAcpRoutingTable::from_configs([(name.to_string(), cfg)]).expect("catalog")
    }

    fn worker_catalog() -> ConfigAcpRoutingTable {
        catalog_from("stub", WORKER_STUB)
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
        let fleet = SubstrateFleet::connect(
            worker_catalog(),
            repo.path().to_path_buf(),
            WorktreesConfig::default(),
            true, // write autonomy granted for this test
        )
        .await;

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
        let fleet = SubstrateFleet::connect(
            worker_catalog(),
            repo.path().to_path_buf(),
            WorktreesConfig::default(),
            true,
        )
        .await;
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

    /// The circuit breaker: once `MAX_CONCURRENT_SUBAGENTS` are live, the next
    /// spawn is rejected (actionable) rather than fanning out unboundedly.
    #[tokio::test]
    async fn spawn_is_capped_at_max_concurrent() {
        let repo = init_repo();
        let fleet = SubstrateFleet::connect(
            catalog_from("idle", IDLE_STUB),
            repo.path().to_path_buf(),
            WorktreesConfig::default(),
            false,
        )
        .await;
        // Fill the fleet to the cap (no worktrees: keep the test light).
        let mut handles = Vec::new();
        for _ in 0..MAX_CONCURRENT_SUBAGENTS {
            let summary = fleet
                .do_spawn(SpawnArgs {
                    agent: "idle".into(),
                    task: "noop".into(),
                    worktree: Some(false),
                    result_schema: None,
                })
                .await
                .expect("spawn under the cap");
            handles.push(summary["handle"].as_str().expect("handle").to_string());
        }
        let err = fleet
            .do_spawn(SpawnArgs {
                agent: "idle".into(),
                task: "one too many".into(),
                worktree: Some(false),
                result_schema: None,
            })
            .await
            .expect_err("spawn beyond the cap is rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("capacity"), "actionable cap message: {msg}");
        assert!(
            msg.contains(&MAX_CONCURRENT_SUBAGENTS.to_string()),
            "names the cap: {msg}"
        );

        // Closing one frees a slot: the cap is a live count, not a lifetime max.
        let freed = handles.remove(0);
        fleet.do_close(&freed).await.expect("close");
        let summary = fleet
            .do_spawn(SpawnArgs {
                agent: "idle".into(),
                task: "now there is room".into(),
                worktree: Some(false),
                result_schema: None,
            })
            .await
            .expect("spawn after a close frees a slot");
        handles.push(summary["handle"].as_str().expect("handle").to_string());

        for handle in &handles {
            fleet.do_close(handle).await.expect("close");
        }
    }

    /// The concurrency guard on the cap: firing more spawns than the cap all at
    /// once must admit exactly the cap and reject the rest — the check and the
    /// reservation are one critical section, so racing spawns can't overshoot.
    /// The rejected spawns bail before leasing a port or launching a child, so
    /// nothing leaks.
    #[tokio::test]
    async fn concurrent_spawns_do_not_overshoot_the_cap() {
        let repo = init_repo();
        let fleet = Arc::new(
            SubstrateFleet::connect(
                catalog_from("idle", IDLE_STUB),
                repo.path().to_path_buf(),
                WorktreesConfig::default(),
                false,
            )
            .await,
        );
        // Fire more spawns than the cap, all at once (worktree=false keeps the
        // test light; the race is between the capacity check and the insert).
        let overshoot = 2usize;
        let mut tasks = Vec::new();
        for i in 0..MAX_CONCURRENT_SUBAGENTS + overshoot {
            let fleet = Arc::clone(&fleet);
            tasks.push(tokio::spawn(async move {
                fleet
                    .do_spawn(SpawnArgs {
                        agent: "idle".into(),
                        task: format!("task {i}"),
                        worktree: Some(false),
                        result_schema: None,
                    })
                    .await
            }));
        }
        let mut admitted = Vec::new();
        let mut rejected = 0usize;
        for task in tasks {
            match task.await.expect("join") {
                Ok(summary) => {
                    admitted.push(summary["handle"].as_str().expect("handle").to_string())
                }
                Err(e) => {
                    assert!(
                        format!("{e:#}").contains("capacity"),
                        "reject reason: {e:#}"
                    );
                    rejected += 1;
                }
            }
        }
        assert_eq!(
            admitted.len(),
            MAX_CONCURRENT_SUBAGENTS,
            "exactly the cap is admitted (no overshoot)"
        );
        assert_eq!(rejected, overshoot, "the excess spawns are all rejected");

        // The reservation accounting reconciled: the fleet is exactly full (no
        // phantom slot lost to a never-released reservation).
        let status = fleet.do_status(None).await.expect("status");
        assert_eq!(
            status["fleet"].as_array().expect("fleet array").len(),
            MAX_CONCURRENT_SUBAGENTS
        );

        // No port leases leaked: rejected spawns bail before `reserve_port`,
        // and each closed subagent drops its lease.
        for handle in &admitted {
            fleet.do_close(handle).await.expect("close");
        }
        let ports_dir = repo.path().join(".bitrouter").join("ports");
        let leaked: Vec<_> = std::fs::read_dir(&ports_dir)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(leaked.is_empty(), "no port leases leaked: {leaked:?}");
    }

    /// The TUI fleet-socket link: handshake policy lands in the flag, and
    /// bridge → TUI messages arrive as parseable NDJSON.
    #[tokio::test]
    async fn tui_link_handshake_and_ndjson_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fleet-tui.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");
        let link = TuiLink::connect(path.to_str().expect("utf8 path"))
            .await
            .expect("connect");
        let (mut stream, _) = listener.accept().await.expect("accept");

        // TUI → bridge: Hello flips the bootstrap policy flag.
        use tokio::io::AsyncWriteExt;
        stream
            .write_all(b"{\"type\":\"hello\",\"bootstrap_approved\":true}\n")
            .await
            .expect("write hello");
        let mut approved = false;
        for _ in 0..100 {
            if link
                .bootstrap_approved
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                approved = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(approved, "Hello's policy reaches the flag");

        // Bridge → TUI: a State message arrives as one parseable line.
        link.send(&crate::fleet::BridgeMsg::State {
            handle: "abc123".into(),
            state: "working".into(),
        })
        .await;
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(stream).lines();
        let line = tokio::time::timeout(std::time::Duration::from_secs(2), lines.next_line())
            .await
            .expect("no timeout")
            .expect("read")
            .expect("one line");
        match serde_json::from_str::<crate::fleet::BridgeMsg>(&line).expect("parse") {
            crate::fleet::BridgeMsg::State { handle, state } => {
                assert_eq!(handle, "abc123");
                assert_eq!(state, "working");
            }
            other => panic!("wrong message: {other:?}"),
        }
    }
}
