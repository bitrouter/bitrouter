//! Pure event + effect types for the TUI reducer.
//!
//! `AppEvent` is what the reducer folds. `Effect` is what the reducer *returns*
//! for the event loop to execute against live sessions (Elm-style: `reduce` stays
//! pure; side effects happen in the loop). `Incoming` is the richer channel
//! message the loop receives (it may carry non-pure handles); the loop converts
//! each `Incoming` into an `AppEvent` before reducing.

use crate::risk::Risk;
use agent_client_protocol::schema::v1::StopReason;
use bitrouter_substrate::translate::{PermissionOutcome, SessionUpdateKind};
use bitrouter_substrate::up::PendingPermission;
use crossterm::event::KeyEvent;

/// A permission option offered to the user, reduced to display data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermOption {
    pub outcome: PermissionOutcome,
    pub label: String,
}

/// A structured file diff (old → new text) carried by a permission request or
/// tool call, ready for the line-diff renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffData {
    pub path: String,
    pub old: String,
    pub new: String,
}

/// Pure event the reducer folds into `AppState`.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// A key was pressed.
    Key(KeyEvent),
    /// A session emitted a translated update.
    Update {
        record_id: String,
        update: SessionUpdateKind,
    },
    /// A session is requesting permission (display data only; the resolvable
    /// handle lives in the loop, keyed by `record_id`).
    Permission {
        record_id: String,
        title: String,
        diff: Option<DiffData>,
        options: Vec<PermOption>,
        /// Classified by the loop from the tool call's kind + locations.
        risk: Risk,
    },
    /// A prompt turn completed; carries the typed ACP stop reason. Feeds the
    /// working/idle state and (with a non-empty diff) the review queue.
    TurnEnded {
        record_id: String,
        stop_reason: StopReason,
    },
    /// The session's agent child exited.
    Exited { record_id: String },
    /// A newly launched session's pane should appear.
    AgentSpawned {
        record_id: String,
        agent_id: String,
        /// The fleet-allocated `PORT` (shown in the roster row), if any.
        port: Option<u16>,
    },
    /// Launching a session failed; surface a transient notice.
    AgentSpawnFailed { agent_id: String, error: String },
    /// A submitted prompt failed to reach the agent; surface it in the pane
    /// (otherwise a dead proxy/agent looks like a silent hang).
    PromptFailed { record_id: String, error: String },
    /// An interactive attach pane opened for `source` (an ACP agent): add a
    /// PTY pane and show it solo. Closing it detaches (kills the interactive
    /// child; the ACP session is untouched).
    PtyAttached { record_id: String, agent_id: String },
    /// A new orchestrator session launched on a PTY: add it to the sessions
    /// panel and show it solo.
    SessionSpawned {
        record_id: String,
        /// The interactive binary hosted in the pane (`claude`, `codex`, …).
        binary: String,
        /// The model the session's LLM traffic is pinned to, if any.
        model: Option<String>,
    },
    /// The loop checked a finished turn's worktree: non-empty diff (and
    /// passing checks) — the agent is ready to review.
    ReviewReady {
        record_id: String,
        files: u64,
        adds: u64,
        dels: u64,
    },
    /// The configured verification checks failed in the agent's worktree.
    /// The failure loops back to the subagent (not the human) while retries
    /// remain; then it surfaces for review with a warning.
    ChecksFailed { record_id: String, output: String },
    /// The agent's full worktree diff, loaded for in-pane review (`D`).
    DiffLoaded { record_id: String, text: String },
    /// A loop-side git operation (merge/apply) finished; surface the outcome
    /// in the pane. Success clears the review flag.
    OpDone {
        record_id: String,
        message: String,
        ok: bool,
    },
    /// Bracketed paste from the outer terminal: one event with the whole
    /// text — never N synthetic keypresses (a multi-line paste must not
    /// submit at every newline).
    Paste(String),
    /// Mouse wheel at terminal cell `(col, row)`. Routed by pointer position
    /// (like [`AppEvent::Click`]): over the sessions panel or the subagents
    /// rail it scrolls that panel; over the detail it pages the focused
    /// pane's scrollback (ACP panes) or forwards arrow presses to the child
    /// (PTY panes).
    Scroll { up: bool, col: u16, row: u16 },
    /// A left-click at terminal cell `(col, row)`. The reducer hit-tests it
    /// against the click zones the renderer recorded for the current frame
    /// (sidebar toggle buttons and roster rows).
    Click { col: u16, row: u16 },
    /// The outer terminal gained (`true`) or lost (`false`) focus. Drives
    /// away-notifications and the done-unseen decay: regaining focus marks
    /// the shown panes seen.
    Focus(bool),
    /// A daemon-reachability probe completed — feeds the status bar's
    /// `serve ●/✗` dot (the loop probes every few seconds).
    ServeStatus { ok: bool },
    /// Periodic UI tick (drives the running-agent spinner animation).
    Tick,
    /// Unconditional quit (input stream ended / terminal gone). Unlike
    /// `Ctrl-C` — which interrupts the focused agent in NORMAL mode — this
    /// always tears down.
    ForceQuit,
    /// An MCP fleet bridge connected over the fleet socket (Unix only). The
    /// reducer answers with a `BridgeHello` carrying standing policy.
    #[cfg(unix)]
    BridgeConnected { conn: u64 },
    /// A subagent the orchestrator spawned via the MCP bridge: mirror it
    /// into the rail as a monitor pane (visible fleet, one roster).
    #[cfg(unix)]
    BridgeSpawned {
        record_id: String,
        agent_id: String,
        port: Option<u16>,
    },
    /// A bridge subagent's Task-shaped state changed
    /// (`working`/`completed`/`failed`).
    #[cfg(unix)]
    BridgeState { record_id: String, state: String },
    /// A bridge connection closed: its mirror panes go dead (their pendings
    /// are already denied bridge-side by the dropped stream).
    #[cfg(unix)]
    BridgeGone { record_ids: Vec<String> },
    /// The orchestrator's `notify_human`: a one-line notice, unattached to any
    /// subagent.
    #[cfg(unix)]
    BridgeNotify { message: String },
    /// The orchestrator's `request_attach`: surface the named subagent as
    /// needing attention (the human drives the attach — mirrors are read-only).
    #[cfg(unix)]
    BridgeRequestAttach { record_id: String },
    /// The orchestrator's `request_review`: flag the subagent's work into the
    /// review queue.
    #[cfg(unix)]
    BridgeRequestReview { record_id: String },
}

/// Side effect the loop performs after a reduce. Keeps `reduce` pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Send `text` as a prompt to the session `record_id`.
    Prompt { record_id: String, text: String },
    /// Resolve the pending permission for `record_id` with `outcome`.
    ResolvePermission {
        record_id: String,
        outcome: PermissionOutcome,
    },
    /// Tear down and exit the TUI.
    Quit,
    /// Ring the terminal bell (background pane needs attention).
    Bell,
    /// Post an outer-terminal notification (OSC 9/99/777, per terminal) —
    /// how the tower reaches the human when the terminal is unfocused.
    Notify { title: String, body: String },
    /// Launch a new agent session (the loop performs the async launch).
    SpawnAgent { agent_id: String },
    /// Launch a new orchestrator session: `binary`'s harness on a fresh PTY
    /// pane, routed through the daemon like the initial `--agent` one.
    SpawnSession { binary: String },
    /// Shut down and remove the session `record_id`.
    CloseAgent { record_id: String },
    /// Route one key press to a PTY pane's child (the loop encodes it via
    /// the pane's emulator, which knows the child's keyboard modes).
    PtyKey { record_id: String, key: KeyEvent },
    /// Route pasted text to a PTY pane's child (bracketed when the inner
    /// app enabled DEC 2004, raw bytes otherwise).
    PtyPaste { record_id: String, text: String },
    /// Cancel the ACP session's in-flight turn (`Ctrl-C` = interrupt the
    /// focused agent, not quit — TUI_SPEC §9/§12).
    CancelTurn { record_id: String },
    /// Attach: relaunch the ACP agent's harness interactively on a PTY in
    /// its worktree (native fidelity for driving one agent; TUI_SPEC §13-B4).
    Attach { record_id: String },
    /// A turn ended cleanly: inspect the agent's worktree (diff + checks) and
    /// report back with `ReviewReady`/`ChecksFailed`.
    CheckReview { record_id: String },
    /// Load the agent's full worktree diff for in-pane review.
    LoadDiff { record_id: String },
    /// Merge the agent's branch into the base repo (human-driven; requires
    /// committed work). Serialized: the loop runs one integration at a time.
    Merge { record_id: String },
    /// Apply the agent's diff onto the base working tree, uncommitted.
    Apply { record_id: String },
    /// The human rejected an orchestrator-owned subagent's diff: carry the
    /// verdict to the owning bridge as the subagent's task outcome
    /// (`changes_requested` + note) — never a prompt (TUI_SPEC_V3 §5).
    ReviewVerdict { record_id: String, note: String },
    /// Answer a bridge's connect with the TUI's standing policy state.
    #[cfg(unix)]
    BridgeHello { conn: u64, bootstrap_approved: bool },
    /// Tell every connected bridge the human approved the bootstrap hook.
    #[cfg(unix)]
    BridgeBootstrapApproved,
}

/// The channel message the loop receives. Carries the real `PendingPermission`
/// handle (not `Clone`), which the loop stashes before deriving `AppEvent`.
pub enum Incoming {
    Update {
        record_id: String,
        update: SessionUpdateKind,
    },
    Permission {
        record_id: String,
        pending: Box<PendingPermission>,
    },
    /// Surface the next queued permission for `record_id` — sent loop-side
    /// after a resolve pops the previous front of the queue.
    PermissionNext {
        record_id: String,
    },
    TurnEnded {
        record_id: String,
        stop_reason: StopReason,
    },
    Exited {
        record_id: String,
    },
    PromptFailed {
        record_id: String,
        error: String,
    },
    ReviewReady {
        record_id: String,
        files: u64,
        adds: u64,
        dels: u64,
    },
    ChecksFailed {
        record_id: String,
        output: String,
    },
    DiffLoaded {
        record_id: String,
        text: String,
    },
    /// Output bytes from a PTY pane's child (fed to its emulator by the loop).
    PtyOutput {
        record_id: String,
        bytes: Vec<u8>,
    },
    /// A PTY pane's child exited (reader hit EOF).
    PtyExited {
        record_id: String,
    },
    /// A background daemon-reachability probe completed.
    ServeStatus {
        ok: bool,
    },
    /// A background `Effect::SpawnAgent` launch succeeded: the loop takes
    /// ownership of the session and its fleet resources.
    Spawned {
        agent_id: String,
        session: Box<bitrouter_substrate::engine::Session>,
        lease: Option<crate::fleet::PortLease>,
        /// Branch-safe agent tag (names the branch with the record16).
        tag: String,
        /// The base repo `HEAD` at spawn — the diff/merge base.
        base_ref: String,
    },
    /// A background `Effect::SpawnAgent` launch failed.
    SpawnFailed {
        agent_id: String,
        error: String,
    },
    /// A background integration (merge/apply) finished.
    OpDone {
        record_id: String,
        message: String,
        ok: bool,
    },
    /// An MCP fleet bridge connected on the fleet socket; the loop keeps the
    /// write half for `BridgeHello`/`Resolve` replies.
    #[cfg(unix)]
    BridgeConnected {
        conn: u64,
        writer: tokio::net::unix::OwnedWriteHalf,
    },
    /// One parsed NDJSON message from bridge connection `conn`.
    #[cfg(unix)]
    Bridge {
        conn: u64,
        msg: crate::fleet::BridgeMsg,
    },
    /// Bridge connection `conn` closed (EOF on its read half).
    #[cfg(unix)]
    BridgeGone {
        conn: u64,
    },
}
