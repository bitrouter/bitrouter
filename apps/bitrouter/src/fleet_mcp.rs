//! The substrate-backed adapter for the fleet capability — the app side of
//! `bitrouter mcp serve --backend fleet` (TUI_SPEC §4).
//!
//! The MCP handler and every tool schema live in `bitrouter-mcp`; this module
//! implements that crate's [`Fleet`]
//! port against `bitrouter_substrate`. All substrate-coupled behavior stays
//! here so the crate never depends on the substrate.
//!
//! The orchestrator profile is **stdio-only** by design: these tools *mutate*
//! (spawn processes, write your repo), so they inherit the orchestrator's
//! process identity instead of riding an unauthenticated HTTP→local path
//! (TUI_SPEC §15-Q2).
//!
//! The internal lifecycle is Task-shaped (MCP Tasks vocabulary — `working /
//! completed / failed`). `spawn` and `prompt` are **non-blocking**: they
//! reserve + launch synchronously (so capacity/budget errors and the handle
//! come back at once), then run the turn in the background and return a
//! `working` ack. The orchestrator polls `subagent_status` for the reply, the
//! typed stop reason, and the worktree diff stat once the turn ends — the
//! summary is stored on the registry entry. A blocking tool would return only
//! when the turn ends, which for a long task outlasts the orchestrator's MCP
//! tool-call timeout: the client reports a false "timed out", retries, and
//! spawns a duplicate subagent (no shipping harness consumes the MCP Tasks
//! extension that would let this be a first-class async task instead of a
//! poll).
//!
//! **Writes are human-gated by default** (TUI_SPEC §5/§7): `apply` and `merge`
//! integrate a subagent's work into the base repository and therefore refuse
//! unless the human started the bridge with `--allow-writes` — an explicit
//! autonomy grant. Subagent permission requests are auto-resolved by risk:
//! reversible + in-worktree allows; everything else escalates in
//! escalation-home priority (TUI_SPEC §5): the orchestrator conversation via a
//! **capability-gated** Tasks `elicitation/create` when the connecting client
//! declared it (forward-compat — no shipping harness does yet, so this is off
//! by default; see `bitrouter_mcp::capabilities::escalation`), otherwise the
//! hosting TUI's decision queue when this bridge runs under `bitrouter tui` (it
//! connects back over the fleet socket, mirroring its subagents into the rail),
//! and denies when headless (logged, never silent).
//!
//! A spend circuit breaker (`--budget-usd`, TUI_SPEC §5) refuses `spawn`/
//! `prompt` once today's machine-wide spend reaches the ceiling.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use bitrouter_mcp::capabilities::escalation::{
    EscalationDecision, EscalationRequest, EscalationState,
};
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
///
/// Public so the MCP server's instruction string can quote the real cap
/// (`BitrouterMcp::builder().subagent_cap(..)`) instead of hardcoding a
/// number that could drift from this one.
pub const MAX_CONCURRENT_SUBAGENTS: usize = 6;

/// The outcome of reading the machine-wide spend the budget ceiling enforces
/// against. Distinguishes a known total (a fresh/empty database reads as
/// `Known(0)`) from an unreadable one — the ceiling **fails closed** on
/// [`SpendReading::Unavailable`] rather than silently treating an unreadable
/// database as `$0`, which would let spawns bypass the budget.
pub enum SpendReading {
    /// A known machine-wide spend total in micro-USD (`0` for an empty/absent
    /// database — a fresh install legitimately hasn't spent anything).
    Known(u64),
    /// A metering database is configured but couldn't be read (config, path,
    /// connection, or query error). Spend is unknown, so the ceiling refuses.
    Unavailable,
}

/// Reads the machine-wide spend figure the budget ceiling enforces against.
/// App-side so `bitrouter-mcp` stays storage-agnostic; a metering-backed impl
/// ([`MeteringSpend`]) and a test stub both satisfy it.
#[async_trait::async_trait]
pub trait SpendSnapshot: Send + Sync {
    /// Today's machine-wide spend as a [`SpendReading`]: `Known(micro_usd)`
    /// when readable (an absent/empty database reads as `Known(0)`), or
    /// `Unavailable` when a configured database can't be read.
    async fn spent_micro_usd(&self) -> SpendReading;
}

/// [`SpendSnapshot`] over the local metering database — the same read-side the
/// `fleet_cost` tool uses. Reports **today's** machine-wide spend (UTC day),
/// which is the figure the budget ceiling is enforced and reported against.
pub struct MeteringSpend {
    source: crate::paths::ConfigSource,
}

impl MeteringSpend {
    /// Read spend from the metering database resolved by `source`.
    pub fn new(source: crate::paths::ConfigSource) -> Self {
        Self { source }
    }
}

#[async_trait::async_trait]
impl SpendSnapshot for MeteringSpend {
    async fn spent_micro_usd(&self) -> SpendReading {
        use crate::metering::reader::ReadSide;
        use crate::metering::store::TimeWindow;
        match crate::metering::reader::read_side(&self.source).await {
            // A fresh install with no database has legitimately spent nothing.
            ReadSide::Absent => SpendReading::Known(0),
            // Configured but unreadable → fail closed (don't guess $0).
            ReadSide::Unavailable => SpendReading::Unavailable,
            ReadSide::Store(store) => match store.spend_summary(TimeWindow::Today).await {
                Ok(today) => SpendReading::Known(today.spend_micro_usd),
                Err(_) => SpendReading::Unavailable,
            },
        }
    }
}

/// The spend circuit breaker (TUI_SPEC §5): a machine-wide spend ceiling that
/// `spawn_subagent` / `prompt_subagent` refuse past, so an orchestrator can't
/// burn through an unbounded budget in one window. The enforced figure is
/// **today's machine-wide spend** (the same `fleet_cost.today` value) — it is
/// *not* scoped to this session, so other spend today counts against it, and a
/// new UTC day is the natural "fresh window".
pub struct BudgetCeiling {
    ceiling_micro_usd: u64,
    spend: Arc<dyn SpendSnapshot>,
}

impl BudgetCeiling {
    /// A ceiling of `ceiling_micro_usd`, enforced against `spend`.
    pub fn new(ceiling_micro_usd: u64, spend: Arc<dyn SpendSnapshot>) -> Self {
        Self {
            ceiling_micro_usd,
            spend,
        }
    }

    /// `Ok(())` when a spawn/prompt may proceed; an actionable `Err` when
    /// today's machine-wide spend has reached the ceiling, or when spend can't
    /// be read at all. An **absent** metering database (fresh install) counts as
    /// `$0` and never blocks; an **unreadable** one (config/permission/
    /// corruption) **fails closed** — a ceiling that silently read an unreadable
    /// database as `$0` would let an orchestrator spend clean past it.
    async fn check(&self) -> Result<()> {
        let spent = match self.spend.spent_micro_usd().await {
            SpendReading::Known(spent) => spent,
            SpendReading::Unavailable => anyhow::bail!(
                "budget ceiling {} is set, but today's machine-wide spend can't be read from \
                 the metering database — refusing to spawn/prompt rather than risk spending \
                 past the ceiling. Check the metering database (path/permissions), or restart \
                 the bridge without --budget-usd.",
                crate::metering::fmt_usd(self.ceiling_micro_usd),
            ),
        };
        if spent >= self.ceiling_micro_usd {
            anyhow::bail!(
                "budget ceiling {} reached; current spend {} — raise it with --budget-usd, or \
                 start a fresh window (the ceiling applies to today's machine-wide spend).",
                crate::metering::fmt_usd(self.ceiling_micro_usd),
                crate::metering::fmt_usd(spent),
            );
        }
        Ok(())
    }
}

/// Convert a `--budget-usd` dollar amount to micro-USD, rejecting a
/// non-positive, non-finite, or sub-micro ceiling — any of which would refuse
/// every spawn, which is never what the operator meant.
pub fn budget_usd_to_micro(usd: f64) -> Result<u64> {
    if !usd.is_finite() || usd <= 0.0 {
        anyhow::bail!("--budget-usd must be a positive dollar amount (got {usd})");
    }
    let micro = (usd * 1_000_000.0).round() as u64;
    if micro == 0 {
        // A positive-but-sub-micro amount (e.g. 0.0000004) rounds to a $0
        // ceiling, which would refuse every spawn — reject it like `<= 0`.
        anyhow::bail!(
            "--budget-usd {usd} rounds to a $0 ceiling (below one micro-USD) and would refuse \
             every spawn; use at least $0.000001."
        );
    }
    Ok(micro)
}

/// A [`SpendSnapshot`] whose reported spend is settable at runtime — the test
/// double for the budget circuit breaker (no metering database needed).
#[cfg(test)]
struct StubSpend(std::sync::atomic::AtomicU64);

#[cfg(test)]
impl StubSpend {
    fn new(micro: u64) -> Arc<Self> {
        Arc::new(Self(std::sync::atomic::AtomicU64::new(micro)))
    }
    // Only the unix-gated `e2e_tests` module flips the spend mid-run; on
    // non-unix that module is `cfg`'d out, so gate the setter to match — an
    // ungated `set` would be dead code (and `-D warnings`) on Windows.
    #[cfg(unix)]
    fn set(&self, micro: u64) {
        self.0.store(micro, Ordering::SeqCst);
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl SpendSnapshot for StubSpend {
    async fn spent_micro_usd(&self) -> SpendReading {
        SpendReading::Known(self.0.load(Ordering::SeqCst))
    }
}

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
    /// The finished turn's summary (reply / result / stop reason / diff stat),
    /// stored by the background runner when the turn ends so `subagent_status`
    /// can return it. `None` while `state == "working"`.
    outcome: Option<serde_json::Value>,
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
/// `Clone` is a cheap `Arc` bump — the background turn runners
/// (`spawn_background` / `prompt_background`) own a handle to store their
/// summaries back on the registry when the turn ends.
#[derive(Clone)]
pub struct SubstrateFleet {
    inner: Arc<FleetInner>,
}

struct FleetInner {
    catalog: ConfigAcpRoutingTable,
    base_repo: PathBuf,
    worktrees: WorktreesConfig,
    /// Human-granted write autonomy (`--allow-writes`).
    allow_writes: bool,
    /// Optional spend circuit breaker (`--budget-usd`). `None` = unlimited.
    budget: Option<BudgetCeiling>,
    /// Shared escalation seam (capability-gated Tasks elicitation). Populated
    /// handler-side at the first fleet tool call; read from the permission path
    /// to decide whether a gated permission routes to the orchestrator
    /// conversation instead of the human-bridge / deny fallback. `None` when the
    /// seam isn't wired (default behavior unchanged).
    escalation: Option<Arc<EscalationState>>,
    /// Gateway MCP descriptors (`bitrouter_tools`/`bitrouter_skills`, see
    /// `crate::gateways`) passed to every spawned subagent's `session/new`,
    /// so subagents reach the same tool/skill surface as the orchestrator.
    subagent_mcp: Vec<agent_client_protocol::schema::v1::McpServer>,
    /// The live subagent registry, under one lock. Also serializes
    /// integration: `apply`/`merge` hold this lock, so branches integrate one
    /// at a time.
    registry: tokio::sync::Mutex<Registry>,
    /// Slots claimed by spawns still launching (a claim is released either when
    /// the subagent is inserted or when the reservation guard drops). Counted
    /// against the cap alongside `registry.agents.len()`. An atomic (not a
    /// registry field) so [`Reservation`]'s `Drop` can release a slot without
    /// an async lock — that's what makes the reservation cancel-safe.
    reserving: Arc<AtomicUsize>,
    /// Live link back to the hosting TUI over the fleet socket, when this
    /// bridge was launched under `bitrouter tui` (Unix): mirrors the fleet
    /// into the rail and routes gated permissions to the human's queue.
    #[cfg(unix)]
    link: Option<Arc<TuiLink>>,
}

/// The live subagent map. Held under the fleet mutex; the spawn-reservation
/// counter lives beside it on [`FleetInner`] (an atomic), and the capacity
/// check reads both while holding this lock so N concurrent `spawn_subagent`
/// calls can't each pass the check and overshoot [`MAX_CONCURRENT_SUBAGENTS`].
#[derive(Default)]
struct Registry {
    /// handle (record16) → subagent.
    agents: HashMap<String, Subagent>,
}

/// RAII claim on one cap slot. Created (after the capacity check) under the
/// fleet lock, which increments `reserving`; its `Drop` decrements again unless
/// the reservation was [`commit`](Reservation::commit)ted — so a spawn future
/// cancelled *between* the reservation and the registry insert can't leak a
/// slot (PR-2 review finding 4: the release used to be a manual `reserving -=
/// 1`, which a drop would skip).
struct Reservation {
    reserving: Arc<AtomicUsize>,
    committed: bool,
}

impl Reservation {
    /// Claim a slot: bump the reserving counter. The caller must already have
    /// verified capacity under the fleet lock (and hold it across this call) so
    /// the check-and-reserve is one critical section.
    fn claim(reserving: Arc<AtomicUsize>) -> Self {
        reserving.fetch_add(1, Ordering::SeqCst);
        Self {
            reserving,
            committed: false,
        }
    }

    /// Convert the reservation into a live registry entry: the caller has
    /// already `fetch_sub`'d the counter under the lock alongside the insert, so
    /// `Drop` must not decrement a second time.
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.committed {
            self.reserving.fetch_sub(1, Ordering::SeqCst);
        }
    }
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
    /// The human's review verdicts by handle (TUI_SPEC_V3 §5): the note the
    /// TUI sent with `changes_requested`. Surfaced as the subagent's task
    /// outcome in `subagent_status`.
    verdicts: tokio::sync::Mutex<HashMap<String, String>>,
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
            verdicts: tokio::sync::Mutex::new(HashMap::new()),
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
                    Ok(crate::fleet::TuiMsg::ReviewVerdict {
                        handle,
                        verdict,
                        note,
                    }) => {
                        // The human's review verdict is the subagent's task
                        // outcome (TUI_SPEC_V3 §5): recorded here, surfaced
                        // by `subagent_status` for the orchestrator.
                        reader_link
                            .verdicts
                            .lock()
                            .await
                            .insert(handle, format!("{verdict}: {note}"));
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
        budget: Option<BudgetCeiling>,
        escalation: Option<Arc<EscalationState>>,
        subagent_mcp: Vec<agent_client_protocol::schema::v1::McpServer>,
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
            budget,
            escalation,
            subagent_mcp,
            registry: tokio::sync::Mutex::new(Registry::default()),
            reserving: Arc::new(AtomicUsize::new(0)),
            #[cfg(unix)]
            link,
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Reserve a fleet slot (budget + capacity gated) and launch + register the
    /// subagent — the **synchronous** half of a spawn, so budget/capacity errors
    /// and the handle come back to the caller before any turn runs.
    async fn reserve_and_launch(&self, args: SpawnArgs) -> Result<Launched> {
        let inner = &self.inner;
        // Spend circuit breaker (TUI_SPEC §5): refuse to launch once the
        // machine-wide budget ceiling is reached. Checked before reserving a
        // slot so an over-budget spawn does no work at all.
        if let Some(budget) = &inner.budget {
            budget.check().await?;
        }
        // Circuit breaker: cap the live fleet so the orchestrator integrates
        // or closes before fanning out unboundedly (TUI_SPEC §5). Reserve a
        // slot atomically — the capacity check and the reservation are one
        // critical section — so N concurrent spawns can't all pass the check
        // and overshoot. The `Reservation` guard releases the slot on drop
        // (including a cancelled spawn future), and `launch_and_register`
        // commits it when the subagent is inserted — so a slot never leaks.
        let reservation = {
            let reg = inner.registry.lock().await;
            if reg.agents.len() + inner.reserving.load(Ordering::SeqCst) >= MAX_CONCURRENT_SUBAGENTS
            {
                anyhow::bail!(
                    "fleet at capacity: {MAX_CONCURRENT_SUBAGENTS} subagents already running. \
                     Integrate or close one (merge_subagent / apply_subagent / close_subagent) \
                     before spawning more."
                );
            }
            Reservation::claim(Arc::clone(&inner.reserving))
        };
        // Launch under the guard: any failure (or a cancelled future) drops the
        // reservation and releases the slot; success commits it inside
        // `launch_and_register` alongside the registry insert.
        self.launch_and_register(args, reservation).await
    }

    /// Run a launched subagent's opening turn to completion and assemble its
    /// summary — the **blocking** half of a spawn, driven in the background by
    /// [`spawn_background`](Self::spawn_background).
    async fn run_opening_turn(&self, launched: Launched) -> Result<serde_json::Value> {
        let Launched {
            handle,
            session,
            task,
            contract,
            bootstrap_skipped,
        } = launched;
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

    /// Non-blocking spawn (TUI_SPEC §4): reserve + launch synchronously (so the
    /// handle and any capacity/budget error come back now), then drive the
    /// opening turn in the **background**, storing its summary on the registry
    /// entry for `subagent_status` to return. This is what stops a long task
    /// from outlasting the orchestrator's MCP tool-call timeout — the failure
    /// that used to be reported as "spawn timed out", retried, and end up
    /// spawning a duplicate subagent.
    async fn spawn_background(&self, args: SpawnArgs) -> Result<serde_json::Value> {
        let launched = self.reserve_and_launch(args).await?;
        let handle = launched.handle.clone();
        let (agent_id, worktree, branch, _base_ref, port) = self.meta(&handle).await?;
        let this = self.clone();
        let runner = handle.clone();
        tokio::spawn(async move {
            let result = this.run_opening_turn(launched).await;
            let _ = this.complete_turn(&runner, result).await;
        });
        Ok(serde_json::json!({
            "handle": handle,
            "agent": agent_id,
            "state": "working",
            "worktree": worktree,
            "branch": branch,
            "port": port,
            "note": format!(
                "launched and running in the background. Poll subagent_status(\"{handle}\") for \
                 its state, reply, and diff — state becomes \"completed\" when the turn ends; it \
                 also appears in the fleet rail for the human. Do not re-spawn if this seems slow: \
                 the subagent is already running."
            ),
        }))
    }

    /// Finalize a turn: store its summary, **then** flip the terminal state.
    /// Order matters — `subagent_status` reports a subagent as done the moment
    /// its state leaves `working`, so the summary must be visible *before* the
    /// state flips, or a poll can catch `completed` with no reply/diff yet (the
    /// race the macOS CI runner exposed). Returns the turn's `result` so the
    /// blocking test wrappers can propagate it.
    async fn complete_turn(
        &self,
        handle: &str,
        result: Result<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let (state, outcome) = match &result {
            Ok(summary) => ("completed", summary.clone()),
            Err(e) => (
                "failed",
                serde_json::json!({ "state": "failed", "error": format!("{e:#}") }),
            ),
        };
        // Store the summary while the state is still `working`…
        if let Some(sub) = self.inner.registry.lock().await.agents.get_mut(handle) {
            sub.outcome = Some(outcome);
        }
        // …then flip the state (which also mirrors to the TUI rail).
        self.set_state(handle, state).await;
        result
    }

    /// Launch the subagent and register it, converting the caller's
    /// `reservation` into a live entry (counter `fetch_sub` + insert, one
    /// critical section) so the live count never overshoots the cap. Any
    /// failure before the insert returns `Err` with the reservation still
    /// armed — its `Drop` releases the slot — and the freshly-leased port /
    /// half-built session drop on the unwind.
    async fn launch_and_register(
        &self,
        args: SpawnArgs,
        reservation: Reservation,
    ) -> Result<Launched> {
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
            // Subagents get the gateway servers (tools/skills) in their
            // `session/new` — the same surface the orchestrator has.
            mcp_servers: inner.subagent_mcp.clone(),
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
            // Convert the reservation into a live entry atomically: the slot
            // moves from `reserving` to `agents` under one lock (release the
            // counter, insert the agent, then `commit` so the guard's Drop
            // won't double-release), so a concurrent spawn never sees the slot
            // double-counted or dropped.
            inner.reserving.fetch_sub(1, Ordering::SeqCst);
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
                    outcome: None,
                },
            );
            reservation.commit();
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

    /// Ready a subagent for a new turn: budget-gated, refuse while it's still
    /// working (one turn per ACP session — concurrent turns would interleave),
    /// clear the prior summary + review verdict, and hand back its session.
    async fn prompt_prepare(&self, handle: &str) -> Result<Arc<Session>> {
        // A prompt turn also spends, so it honors the same ceiling as spawn.
        if let Some(budget) = &self.inner.budget {
            budget.check().await?;
        }
        let session = {
            let mut reg = self.inner.registry.lock().await;
            let sub = reg
                .agents
                .get_mut(handle)
                .with_context(|| format!("no subagent with handle '{handle}'"))?;
            if sub.state == "working" {
                anyhow::bail!(
                    "subagent '{handle}' is still working — wait for it to reach `completed` \
                     (poll subagent_status) before prompting again."
                );
            }
            sub.state = "working";
            sub.outcome = None; // the new turn supersedes the stored summary
            Arc::clone(&sub.session)
        };
        // Re-prompting consumes the human's review verdict (TUI_SPEC_V3 §5):
        // the orchestrator has acted on `changes_requested`, so the revision
        // turn's lifecycle state must be observable again in
        // `subagent_status` — a sticky verdict would mask it forever.
        #[cfg(unix)]
        if let Some(link) = &self.inner.link {
            link.verdicts.lock().await.remove(handle);
        }
        Ok(session)
    }

    /// Non-blocking follow-up prompt: the same background-turn model as spawn,
    /// so a long revision turn never outlasts the caller's tool-call timeout.
    async fn prompt_background(&self, args: PromptArgs) -> Result<serde_json::Value> {
        let session = self.prompt_prepare(&args.handle).await?;
        let this = self.clone();
        let handle = args.handle.clone();
        let text = args.text;
        tokio::spawn(async move {
            let result = this.run_blocking_turn(&handle, session, &text, None).await;
            let _ = this.complete_turn(&handle, result).await;
        });
        Ok(serde_json::json!({
            "handle": args.handle,
            "state": "working",
            "note": format!(
                "prompt delivered — running in the background. Poll subagent_status(\"{}\") for \
                 the reply and diff.",
                args.handle
            ),
        }))
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
        // State transitions are the caller's job (`complete_turn`): the terminal
        // state must flip only *after* the summary is stored, or a poll keyed on
        // `state != "working"` can catch `completed` with no summary yet.
        let (response, reply) = collect_turn(&session, &task).await?;
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
        // The human's review verdicts (TUI_SPEC_V3 §5), merged into each
        // snapshot as the subagent's task outcome.
        #[cfg(unix)]
        let verdicts = match &self.inner.link {
            Some(link) => link.verdicts.lock().await.clone(),
            None => HashMap::new(),
        };
        #[cfg(not(unix))]
        let verdicts: HashMap<String, String> = HashMap::new();
        let reg = self.inner.registry.lock().await;
        match handle {
            Some(h) => {
                let sub = reg
                    .agents
                    .get(h)
                    .with_context(|| format!("no subagent with handle '{h}'"))?;
                Ok(snapshot(h, sub, verdicts.get(h).map(String::as_str)).await)
            }
            None => {
                let mut fleet = Vec::new();
                for (h, sub) in reg.agents.iter() {
                    fleet.push(snapshot(h, sub, verdicts.get(h).map(String::as_str)).await);
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
            outcome,
        } = sub;
        let only = match Arc::try_unwrap(session) {
            Ok(only) => only,
            Err(session) => {
                // A turn is in flight — the background runner still holds a
                // session `Arc` — so put the entry back; removing it here would
                // orphan the child process and its worktree lease.
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
                        outcome,
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

impl SubstrateFleet {
    /// Deliver a human-facing message over the TUI fleet socket, returning a
    /// `delivered` JSON ack. When headless (no TUI attached), return a note
    /// that no human is watching instead of erroring — a subagent with no human
    /// in the loop should hear that, not a failure. Best-effort like the fleet
    /// mirror messages: a dropped socket is not the orchestrator's problem.
    async fn send_to_human(
        &self,
        msg: crate::fleet::BridgeMsg,
        delivered: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        #[cfg(unix)]
        if let Some(link) = &self.inner.link {
            link.send(&msg).await;
            let mut out = delivered;
            out["delivered"] = serde_json::json!(true);
            return Ok(out);
        }
        let _ = (&msg, &delivered);
        Ok(serde_json::json!({
            "delivered": false,
            "note": "no human is attached (headless bridge) — nothing was shown",
        }))
    }

    /// Validate `handle` against the live registry before an escalation that
    /// names it. `request_attach`/`request_review` reach the human about a
    /// specific subagent, so an unknown/stale handle must be a `ToolError`
    /// (like `subagent_status`/`subagent_diff`) rather than a silent
    /// `{delivered:true}` for an agent that doesn't exist (PR-2 review
    /// finding 1).
    async fn ensure_handle(&self, handle: &str) -> Result<(), ToolError> {
        if self.inner.registry.lock().await.agents.contains_key(handle) {
            Ok(())
        } else {
            Err(ToolError::new(format!(
                "no subagent with handle '{handle}'"
            )))
        }
    }
}

#[async_trait::async_trait]
impl bitrouter_mcp::capabilities::human::HumanBridge for SubstrateFleet {
    async fn notify(&self, message: &str) -> Result<serde_json::Value, ToolError> {
        self.send_to_human(
            crate::fleet::BridgeMsg::Notify {
                message: message.to_string(),
            },
            serde_json::json!({ "notice": message }),
        )
        .await
    }

    async fn request_attach(&self, handle: &str) -> Result<serde_json::Value, ToolError> {
        self.ensure_handle(handle).await?;
        self.send_to_human(
            crate::fleet::BridgeMsg::RequestAttach {
                handle: handle.to_string(),
            },
            serde_json::json!({ "requested": "attach", "handle": handle }),
        )
        .await
    }

    async fn request_review(&self, handle: &str) -> Result<serde_json::Value, ToolError> {
        self.ensure_handle(handle).await?;
        // NOTE (PR-2 review finding 2, documented descope): under `bitrouter
        // tui` this mirrors the subagent into the human's review queue, but a
        // *bridge-mirrored* subagent has no `rt.fleet.meta` entry there, so the
        // queue's D/m/p verbs (load-diff / merge / apply) no-op on it. That's
        // acceptable: review-queue actions on a bridge-mirrored subagent are
        // advisory — the human drives merge/apply from the owning process (this
        // bridge's `merge_subagent`/`apply_subagent`, or the orchestrator's).
        self.send_to_human(
            crate::fleet::BridgeMsg::RequestReview {
                handle: handle.to_string(),
            },
            serde_json::json!({ "requested": "review", "handle": handle }),
        )
        .await
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
        self.spawn_background(args).await.map_err(to_tool_error)
    }

    async fn prompt(&self, args: PromptArgs) -> Result<serde_json::Value, ToolError> {
        self.prompt_background(args).await.map_err(to_tool_error)
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
/// reversible + in-worktree ⇒ allow-once; everything else, in escalation-home
/// priority (TUI_SPEC §5): (1) the orchestrator conversation via a
/// capability-gated Tasks `elicitation/create`, when the connecting client
/// declared it — forward-compat, off for every shipping harness today;
/// (2) otherwise the hosting TUI's decision queue when this bridge is linked;
/// (3) otherwise deny (headless, no human in the loop). Every decision is
/// logged to stderr (never silent).
fn spawn_auto_policy(inner: &Arc<FleetInner>, session: &Arc<Session>, handle: String) {
    let mut perms = session.permissions();
    let workroot = inner.base_repo.clone();
    let escalation = inner.escalation.clone();
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
                    // (1) Capability-gated escalation to the orchestrator
                    // conversation. `escalate` returns `None` unless a client
                    // declared the Tasks/elicitation capability (none do yet),
                    // so this falls through to the existing path by default.
                    if let Some(escalation) = &escalation
                        && escalation.can_escalate()
                    {
                        let decision = escalation
                            .escalate(EscalationRequest {
                                subagent: handle.clone(),
                                tool_title: title.clone(),
                            })
                            .await;
                        match decision {
                            Some(EscalationDecision::Allow) => {
                                tracing::info!(
                                    subagent = %handle, tool = %title,
                                    "allowed by the orchestrator conversation (elicitation)"
                                );
                                let selected =
                                    select_option(PermissionOutcome::AllowOnce, &pending.options);
                                pending.resolve(selected);
                                continue;
                            }
                            Some(EscalationDecision::Deny) => {
                                tracing::info!(
                                    subagent = %handle, tool = %title,
                                    "denied by the orchestrator conversation (elicitation)"
                                );
                                pending.deny();
                                continue;
                            }
                            // Round-trip failed — fall through to the existing
                            // path rather than fail open.
                            None => {}
                        }
                    }
                    // (2) The hosting TUI's decision queue, when linked.
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
                                pending.deny();
                            }
                        }
                        continue;
                    }
                    // (3) Headless: deny.
                    tracing::warn!(subagent = %handle, tool = %title, "denied (high risk, no human in the loop)");
                    pending.deny();
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
async fn snapshot(handle: &str, sub: &Subagent, verdict: Option<&str>) -> serde_json::Value {
    // Once the background turn ends, its stored summary (reply / result /
    // stop_reason / diff_stat) IS the snapshot — that's how the orchestrator
    // reads the result it used to get inline from a blocking spawn. While the
    // turn is still working there is no summary yet, so report a live view of
    // the lifecycle + current worktree diff.
    let mut snap = match &sub.outcome {
        Some(outcome) => outcome.clone(),
        None => serde_json::json!({
            "state": sub.state,
            "worktree": sub.worktree,
            "branch": sub.branch,
            "port": sub.port.as_ref().map(|l| l.port()),
            "diff_stat": match &sub.worktree {
                Some(wt) => crate::fleet::diff_stat(wt, &sub.base_ref).await,
                None => None,
            },
        }),
    };
    // Identity fields, always present regardless of the outcome shape (a
    // `failed` summary carries only state + error).
    snap["handle"] = serde_json::json!(handle);
    snap["agent"] = serde_json::json!(sub.agent_id);
    // A human review verdict is the task outcome — it outranks the lifecycle
    // state so the orchestrator can't miss it.
    match verdict {
        Some(v) => {
            snap["state"] = serde_json::json!("changes_requested");
            snap["review_verdict"] = serde_json::json!(v);
        }
        None => {
            snap["review_verdict"] = serde_json::Value::Null;
        }
    }
    snap
}

/// Blocking spawn/prompt for the lifecycle tests: reserve + launch + run the
/// turn to completion inline, so a test can assert on the finished summary and
/// drive apply/merge/diff without polling. Production goes through the
/// non-blocking [`spawn_background`](SubstrateFleet::spawn_background) /
/// [`prompt_background`](SubstrateFleet::prompt_background); the non-blocking
/// contract is covered by its own tests below. Gated to `unix` like the
/// `e2e_tests` module that calls it — the bash ACP stubs are unix-only, so on
/// other platforms these would be callerless dead code.
#[cfg(all(test, unix))]
impl SubstrateFleet {
    async fn do_spawn(&self, args: SpawnArgs) -> Result<serde_json::Value> {
        let launched = self.reserve_and_launch(args).await?;
        let handle = launched.handle.clone();
        let result = self.run_opening_turn(launched).await;
        self.complete_turn(&handle, result).await
    }

    async fn do_prompt(&self, args: PromptArgs) -> Result<serde_json::Value> {
        let session = self.prompt_prepare(&args.handle).await?;
        let result = self
            .run_blocking_turn(&args.handle, session, &args.text, None)
            .await;
        self.complete_turn(&args.handle, result).await
    }
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
            None,       // no budget ceiling in this test
            None,       // no escalation seam in this test
            Vec::new(), // no gateway descriptors in this test
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
            None,       // no budget ceiling in this test
            None,       // no escalation seam in this test
            Vec::new(), // no gateway descriptors in this test
        )
        .await;
        assert!(granted.require_write_grant("merge_subagent").is_ok());
    }

    /// Headless (no TUI socket): `notify_human` (which names no subagent)
    /// doesn't error — it reports that no human is attached, so a subagent
    /// hears "nobody's watching" rather than a failure.
    #[tokio::test]
    async fn notify_human_reports_headless_without_a_tui_link() {
        use bitrouter_mcp::capabilities::human::HumanBridge;
        let fleet = SubstrateFleet::connect(
            ConfigAcpRoutingTable::from_configs(std::iter::empty()).expect("empty catalog"),
            PathBuf::from("/tmp"),
            WorktreesConfig::default(),
            false,
            None,       // no budget ceiling in this test
            None,       // no escalation seam in this test
            Vec::new(), // no gateway descriptors in this test
        )
        .await;
        let out = fleet.notify("heads up").await.expect("notify ok");
        assert_eq!(
            out["delivered"], false,
            "headless: nothing delivered: {out}"
        );
        assert!(
            out["note"].as_str().is_some_and(|n| n.contains("no human")),
            "explains why: {out}"
        );
    }

    /// PR-2 review finding 1: `request_attach`/`request_review` name a specific
    /// subagent, so a stale/unknown handle is a `ToolError` naming the bad
    /// handle — not a silent ack for an agent that doesn't exist.
    #[tokio::test]
    async fn attach_and_review_reject_unknown_handles() {
        use bitrouter_mcp::capabilities::human::HumanBridge;
        let fleet = SubstrateFleet::connect(
            ConfigAcpRoutingTable::from_configs(std::iter::empty()).expect("empty catalog"),
            PathBuf::from("/tmp"),
            WorktreesConfig::default(),
            false,
            None,       // no budget ceiling in this test
            None,       // no escalation seam in this test
            Vec::new(), // no gateway descriptors in this test
        )
        .await;
        for res in [
            fleet.request_attach("ffffffffffffffff").await,
            fleet.request_review("ffffffffffffffff").await,
        ] {
            let err = res.expect_err("an unknown handle must error, not ack");
            assert!(
                err.0.contains("ffffffffffffffff"),
                "names the bad handle: {}",
                err.0
            );
        }
    }

    #[test]
    fn budget_usd_to_micro_scales_and_rejects_unusable() {
        assert_eq!(budget_usd_to_micro(2.5).expect("scales"), 2_500_000);
        assert_eq!(budget_usd_to_micro(0.000_001).expect("rounds"), 1);
        assert!(
            budget_usd_to_micro(0.0).is_err(),
            "$0 would block every spawn"
        );
        assert!(
            budget_usd_to_micro(0.000_000_4).is_err(),
            "a sub-micro amount rounds to a $0 ceiling → every spawn blocked"
        );
        assert!(budget_usd_to_micro(-1.0).is_err());
        assert!(budget_usd_to_micro(f64::NAN).is_err());
        assert!(budget_usd_to_micro(f64::INFINITY).is_err());
    }

    #[tokio::test]
    async fn budget_ceiling_allows_under_and_refuses_at_or_over() {
        // $3 spent against a $10 ceiling → proceed.
        let under = BudgetCeiling::new(10_000_000, StubSpend::new(3_000_000));
        under.check().await.expect("under budget proceeds");

        // At the ceiling → refuse with an actionable message (>= is over-budget).
        let at = BudgetCeiling::new(10_000_000, StubSpend::new(10_000_000));
        let err = at.check().await.expect_err("at the ceiling refuses");
        let msg = format!("{err:#}");
        assert!(msg.contains("budget ceiling"), "names the breaker: {msg}");
        assert!(msg.contains("--budget-usd"), "actionable: {msg}");
    }

    #[tokio::test]
    async fn budget_ceiling_fails_closed_when_spend_is_unreadable() {
        // A configured-but-unreadable metering database must REFUSE the spawn,
        // not silently proceed as if $0 were spent — otherwise the ceiling is a
        // no-op whenever the bridge can't read the DB (permissions/corruption),
        // and an orchestrator could spend clean past it.
        struct Unreadable;
        #[async_trait::async_trait]
        impl SpendSnapshot for Unreadable {
            async fn spent_micro_usd(&self) -> SpendReading {
                SpendReading::Unavailable
            }
        }
        let budget = BudgetCeiling::new(1_000_000, Arc::new(Unreadable));
        let err = budget
            .check()
            .await
            .expect_err("unreadable spend must fail closed, not proceed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("can't be read"),
            "explains why it refused: {msg}"
        );
        assert!(msg.contains("--budget-usd"), "actionable: {msg}");

        // But a fresh/empty database reads as `Known(0)` — legitimately $0, so
        // the first spawn is never blocked by a cold database.
        let fresh = BudgetCeiling::new(1_000_000, StubSpend::new(0));
        fresh
            .check()
            .await
            .expect("an empty database reads as $0 and never blocks the first spawn");
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

    /// A stub that answers `initialize`/`session/new` but never responds to a
    /// prompt — the turn stays in-flight, so `state` stays `working`. Lets the
    /// non-blocking tests observe the working window deterministically.
    const HANG_STUB: &str = r#"
        while read line; do
          id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
          case "$line" in
            *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
            *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
            *session/prompt*) : ;;
          esac
        done
    "#;

    /// Poll `subagent_status` the way an orchestrator does, until the turn
    /// leaves `working`. Bounded so a hung turn fails the test loudly.
    async fn poll_until_done(fleet: &SubstrateFleet, handle: &str) -> serde_json::Value {
        for _ in 0..300 {
            let s = fleet.do_status(Some(handle)).await.expect("status");
            if s["state"] != "working" {
                return s;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("subagent {handle} never left `working`");
    }

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

    /// The production entry (`Fleet::spawn`) is non-blocking: it returns a
    /// `working` ack at once — not the finished summary — so a long turn can't
    /// outlast the caller's MCP tool-call timeout (the false "timed out" that
    /// used to be retried into a duplicate subagent). The result lands on
    /// `subagent_status` when the background turn ends.
    #[tokio::test]
    async fn spawn_is_nonblocking_and_status_carries_the_result() {
        let repo = init_repo();
        let fleet = SubstrateFleet::connect(
            worker_catalog(),
            repo.path().to_path_buf(),
            WorktreesConfig::default(),
            true,
            None,
            None,
            Vec::new(),
        )
        .await;
        let ack = fleet
            .spawn(SpawnArgs {
                agent: "stub".into(),
                task: "write made.txt".into(),
                worktree: None,
                result_schema: None,
            })
            .await
            .expect("spawn ack");
        assert_eq!(
            ack["state"], "working",
            "returns before the turn ends: {ack}"
        );
        assert!(
            ack["note"]
                .as_str()
                .is_some_and(|n| n.contains("subagent_status")),
            "the ack points the orchestrator at the poll surface: {ack}"
        );
        let handle = ack["handle"].as_str().expect("handle").to_string();

        // Poll to completion the way an orchestrator would; the stored summary
        // (reply + stop reason + diff) surfaces on the status snapshot.
        let status = poll_until_done(&fleet, &handle).await;
        assert_eq!(status["state"], "completed");
        assert_eq!(status["stop_reason"], "end_turn");
        assert!(
            status["reply"].as_str().is_some_and(|r| r.contains("done")),
            "reply on status: {status}"
        );
        assert_eq!(status["diff_stat"]["files"], 1, "diff via status: {status}");
        assert_eq!(status["handle"], handle);
        fleet.do_close(&handle).await.expect("close");
    }

    /// One turn per session: a follow-up prompt is refused while the opening
    /// turn is still in flight (two concurrent turns on one ACP session would
    /// interleave). The orchestrator waits for `completed` before prompting.
    #[tokio::test]
    async fn prompt_is_refused_while_the_subagent_is_working() {
        let repo = init_repo();
        let fleet = SubstrateFleet::connect(
            catalog_from("hang", HANG_STUB),
            repo.path().to_path_buf(),
            WorktreesConfig::default(),
            true,
            None,
            None,
            Vec::new(),
        )
        .await;
        let ack = fleet
            .spawn(SpawnArgs {
                agent: "hang".into(),
                task: "noop".into(),
                worktree: Some(false),
                result_schema: None,
            })
            .await
            .expect("spawn ack");
        let handle = ack["handle"].as_str().expect("handle").to_string();
        assert_eq!(ack["state"], "working");
        // The opening turn never completes (hang stub), so state stays working.
        let err = fleet
            .prompt(PromptArgs {
                handle: handle.clone(),
                text: "again".into(),
            })
            .await
            .expect_err("a prompt is refused while a turn is in flight");
        assert!(
            format!("{err:?}").contains("still working"),
            "actionable refusal: {err:?}"
        );
    }

    #[tokio::test]
    async fn spawn_review_merge_close_roundtrip() {
        let repo = init_repo();
        let fleet = SubstrateFleet::connect(
            worker_catalog(),
            repo.path().to_path_buf(),
            WorktreesConfig::default(),
            true,       // write autonomy granted for this test
            None,       // no budget ceiling in this test
            None,       // no escalation seam in this test
            Vec::new(), // no gateway descriptors in this test
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

        // ── human bridge: a *live* handle passes validation (finding 1);
        // headless, it reports delivered:false rather than erroring ──
        {
            use bitrouter_mcp::capabilities::human::HumanBridge;
            let attach = fleet.request_attach(&handle).await.expect("live handle ok");
            assert_eq!(attach["delivered"], false, "headless: {attach}");
        }

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
            None,       // no budget ceiling in this test
            None,       // no escalation seam in this test
            Vec::new(), // no gateway descriptors in this test
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
            None,       // no budget ceiling in this test
            None,       // no escalation seam in this test
            Vec::new(), // no gateway descriptors in this test
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
                None,       // no budget ceiling in this test
                None,       // no escalation seam in this test
                Vec::new(), // no gateway descriptors in this test
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

    /// The budget circuit breaker (TUI_SPEC §5 / PR-3 B1): a spawn/prompt under
    /// the ceiling proceeds; once today's spend reaches it, both are refused
    /// (actionable) and no new session is launched.
    #[tokio::test]
    async fn budget_ceiling_gates_spawn_and_prompt() {
        let repo = init_repo();
        let spend = StubSpend::new(0);
        let fleet = SubstrateFleet::connect(
            catalog_from("idle", IDLE_STUB),
            repo.path().to_path_buf(),
            WorktreesConfig::default(),
            false,
            Some(BudgetCeiling::new(10_000_000, spend.clone())),
            None,       // no escalation seam in this test
            Vec::new(), // no gateway descriptors in this test
        )
        .await;

        // Under budget ($0 of $10): spawn proceeds.
        let summary = fleet
            .do_spawn(SpawnArgs {
                agent: "idle".into(),
                task: "noop".into(),
                worktree: Some(false),
                result_schema: None,
            })
            .await
            .expect("under budget spawns");
        let handle = summary["handle"].as_str().expect("handle").to_string();

        // Reach the ceiling: the next spawn AND a follow-up prompt are refused.
        spend.set(10_000_000);
        let spawn_err = fleet
            .do_spawn(SpawnArgs {
                agent: "idle".into(),
                task: "one too many".into(),
                worktree: Some(false),
                result_schema: None,
            })
            .await
            .expect_err("over-budget spawn is refused");
        assert!(
            format!("{spawn_err:#}").contains("budget ceiling"),
            "actionable: {spawn_err:#}"
        );
        let prompt_err = fleet
            .do_prompt(PromptArgs {
                handle: handle.clone(),
                text: "again".into(),
            })
            .await
            .expect_err("over-budget prompt is refused");
        assert!(
            format!("{prompt_err:#}").contains("budget ceiling"),
            "actionable: {prompt_err:#}"
        );

        // The refused spawn launched nothing: still exactly one live subagent.
        let fleet_snap = fleet.do_status(None).await.expect("status");
        assert_eq!(
            fleet_snap["fleet"].as_array().expect("fleet array").len(),
            1,
            "over-budget spawn did not launch a session"
        );
        fleet.do_close(&handle).await.expect("close");
    }

    /// Unlimited by default: no `--budget-usd` means no ceiling ever blocks.
    #[tokio::test]
    async fn unlimited_budget_default_never_blocks() {
        let repo = init_repo();
        let fleet = SubstrateFleet::connect(
            catalog_from("idle", IDLE_STUB),
            repo.path().to_path_buf(),
            WorktreesConfig::default(),
            false,
            None,       // no ceiling
            None,       // no escalation seam in this test
            Vec::new(), // no gateway descriptors in this test
        )
        .await;
        let summary = fleet
            .do_spawn(SpawnArgs {
                agent: "idle".into(),
                task: "noop".into(),
                worktree: Some(false),
                result_schema: None,
            })
            .await
            .expect("no ceiling → spawns");
        fleet
            .do_close(summary["handle"].as_str().expect("handle"))
            .await
            .expect("close");
    }
}
