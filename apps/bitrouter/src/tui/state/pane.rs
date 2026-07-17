//! Per-agent pane state: the `PaneState` record and its supporting
//! enums (kind, ownership, autonomy tier, tail stream, pending view).

use std::collections::HashMap;

use crate::risk::Risk;
use crate::tui::event::{DiffData, PermOption};
use agent_client_protocol::schema::v1::StopReason;
use bitrouter_substrate::translate::UsageCost;

use super::diff::{Line, diff_lines, parse_rendered_diff};
use super::{SCROLLBACK_CAP, TICKS_PER_SEC, fmt_elapsed};

/// Which stream the mutable tail belongs to. Chunked deltas accumulate here
/// and commit to scrollback only when newline-terminated (two-region model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailKind {
    Message,
    Thought,
}

/// A pending permission surfaced in the pane, as display data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingView {
    pub title: String,
    pub diff: Option<DiffData>,
    pub options: Vec<PermOption>,
    pub risk: Risk,
}

/// Per-agent autonomy tier: which permission requests reach the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Autonomy {
    /// Every request surfaces (default for a fresh, untrusted agent).
    #[default]
    Manual,
    /// Low-risk requests auto-allow; high-risk surface.
    Assisted,
    /// Everything auto-allows (logged, never silent).
    Auto,
}

impl Autonomy {
    /// Cycle Manual → Assisted → Auto → Manual.
    pub(super) fn next(self) -> Self {
        match self {
            Autonomy::Manual => Autonomy::Assisted,
            Autonomy::Assisted => Autonomy::Auto,
            Autonomy::Auto => Autonomy::Manual,
        }
    }

    /// Short label for the rail row and log lines.
    pub fn label(self) -> &'static str {
        match self {
            Autonomy::Manual => "manual",
            Autonomy::Assisted => "assisted",
            Autonomy::Auto => "auto",
        }
    }
}

/// What renders a pane's content (TUI_SPEC_V3 §2's two-kind pane model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneKind {
    /// A read-only subagent monitor, rendered by bitrouter from typed ACP
    /// events: transcript + decision queue + review verbs, no composer.
    #[default]
    Monitor,
    /// A native harness on a PTY (the orchestrator) — rendered by the
    /// terminal backend; keys pass through except the leader.
    Pty,
}

/// Who steers an agent (TUI_SPEC_V3 §4's ownership rule). Capability edges
/// key off this, not the pane kind: an orchestrator-owned session lives in
/// another process, so it can't be cancelled, attached, re-tiered, or closed
/// from here — and review verdicts route back as its task outcome (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ownership {
    /// Human-spawned (the palette hatch, attaches, sessions): steered here.
    #[default]
    Human,
    /// Spawned by an orchestrator via the MCP bridge: steered there.
    Orchestrator,
}

/// One agent pane's state.
#[derive(Debug, Clone)]
pub struct PaneState {
    pub record_id: String,
    pub agent_id: String,
    /// Terse harness tag shown in the pane header (e.g. `claude`, `codex`).
    /// Empty when unknown.
    pub harness: String,
    pub lines: Vec<Line>,
    pub pending: Option<PendingView>,
    pub exited: bool,
    /// Something went wrong in the background (error, exit, gated
    /// permission) and the human hasn't looked yet.
    pub attention: bool,
    /// A turn finished and the human hasn't looked yet — the inbox-unread
    /// state (herdr's `done`). Decays on view: seeing the pane while the
    /// terminal is focused clears it back to idle.
    pub done: bool,
    /// Tick at which the pane last changed actionability bucket; feeds the
    /// rail's time-in-state column.
    pub(super) since: u64,
    /// The bucket `since` was stamped for (change detection in `reduce`).
    pub(super) last_bucket: u8,
    /// Which permission requests reach the user (cycled with `A` on the rail).
    pub autonomy: Autonomy,
    /// Arrival order of the current `pending` (from `AppState.perm_seq`);
    /// the queue orders needs-you rows oldest-first with it.
    pub pending_seq: u64,
    /// `None` = follow the tail; `Some(i)` = pinned with line `i` first visible.
    /// Content-pinned: new output never moves a pinned view.
    pub scroll: Option<usize>,
    /// Inner height (rows) this pane last rendered at; recorded by the UI so
    /// paging moves by exactly one screen (ratatui stateful-render idiom).
    pub viewport: usize,
    /// Context-window occupancy `(used, size)` from the latest upstream
    /// `usage` update; shown in the pane header.
    pub usage: Option<(u64, u64)>,
    /// Cumulative session cost from the latest upstream `usage` update;
    /// shown as the `$` column in the roster.
    pub cost: Option<UsageCost>,
    /// The mutable tail of the two-region streaming model: chunked deltas
    /// accumulate here and commit to `lines` only when newline-terminated,
    /// so a half-formed line never flashes in the scrollback.
    pub tail: Option<(TailKind, String)>,
    /// `Some(lang)` while committed message lines are inside a fenced code
    /// block (streaming fence parser state).
    code_lang: Option<String>,
    /// tool id → last raw diff pushed for it, so a `ToolCallUpdate` repeating
    /// the same diff doesn't duplicate it in the scrollback.
    tool_diffs: HashMap<String, String>,
    /// A prompt turn is in flight (set on send, cleared by `TurnEnded`).
    /// Distinguishes working (spinner) from idle in the rail.
    pub turn_active: bool,
    /// The last turn's typed ACP stop reason.
    pub last_stop: Option<StopReason>,
    /// The fleet-allocated `PORT` for this agent's dev server, if any;
    /// shown in the roster row so N servers stay tellable apart.
    pub port: Option<u16>,
    /// Ready to review (TUI_SPEC §7): the turn ended cleanly with a
    /// non-empty worktree diff (`(files, adds, dels)`). Cleared by
    /// merge/apply/reject or a new prompt.
    pub review: Option<(u64, u64, u64)>,
    /// How many times this turn's failing checks were looped back to the
    /// agent (capped — then the failure surfaces to the human instead).
    pub check_retries: u8,
    /// What renders this pane (ACP monitor vs native PTY).
    pub kind: PaneKind,
    /// Who steers this agent (TUI_SPEC_V3 §4): set at spawn time — `Human`
    /// for hatch spawns/attaches/sessions, `Orchestrator` for bridge spawns.
    pub owner: Ownership,
    /// The model this pane was pinned to at launch (sessions: the `--model`
    /// value), shown in the sessions panel. `None` = the daemon's default.
    pub model: Option<String>,
}

impl PaneState {
    pub fn new(record_id: String, agent_id: String) -> Self {
        Self {
            record_id,
            agent_id,
            harness: String::new(),
            lines: Vec::new(),
            pending: None,
            exited: false,
            attention: false,
            done: false,
            since: 0,
            // Sentinel: no bucket stamped yet — the first reduce stamps
            // `since` with the current tick.
            last_bucket: u8::MAX,
            autonomy: Autonomy::default(),
            pending_seq: 0,
            scroll: None,
            viewport: 0,
            usage: None,
            cost: None,
            tail: None,
            code_lang: None,
            tool_diffs: HashMap::new(),
            turn_active: false,
            last_stop: None,
            port: None,
            review: None,
            check_retries: 0,
            kind: PaneKind::default(),
            owner: Ownership::default(),
            model: None,
        }
    }

    /// A durable orchestrator session: a PTY pane that isn't a transient
    /// interactive attach (attaches belong to their ACP agent, not the
    /// sessions memory).
    pub fn is_session(&self) -> bool {
        self.kind == PaneKind::Pty && !self.record_id.starts_with("attach:")
    }

    pub(super) fn push(&mut self, line: Line) {
        self.lines.push(line);
        if self.lines.len() > SCROLLBACK_CAP {
            let overflow = self.lines.len() - SCROLLBACK_CAP;
            self.lines.drain(0..overflow);
            // Keep a pinned view on the same content as the buffer slides.
            if let Some(s) = &mut self.scroll {
                *s = s.saturating_sub(overflow);
            }
        }
    }

    /// Push a non-stream line (prompt, tool, error, note), committing any
    /// half-formed streamed line first so ordering stays faithful.
    pub(super) fn push_external(&mut self, line: Line) {
        self.flush_tail();
        self.push(line);
    }

    /// Commit the mutable tail (if any) to the scrollback even without a
    /// trailing newline — called when the stream is interrupted or ends.
    pub(super) fn flush_tail(&mut self) {
        if let Some((kind, buf)) = self.tail.take()
            && !buf.is_empty()
        {
            self.commit_stream_line(kind, buf);
        }
    }

    /// Fold a streamed text delta into the tail, committing every
    /// newline-terminated line. A kind switch (message ↔ thought) commits the
    /// other stream's partial line first.
    pub(super) fn stream(&mut self, kind: TailKind, text: &str) {
        if let Some((k, _)) = &self.tail
            && *k != kind
        {
            self.flush_tail();
        }
        let mut buf = match self.tail.take() {
            Some((_, b)) => b,
            None => String::new(),
        };
        buf.push_str(text);
        // Commit complete lines; keep the unterminated remainder as the tail.
        while let Some(pos) = buf.find('\n') {
            let line: String = buf.drain(..=pos).collect();
            let line = line
                .trim_end_matches('\n')
                .trim_end_matches('\r')
                .to_string();
            self.commit_stream_line(kind, line);
        }
        if !buf.is_empty() {
            self.tail = Some((kind, buf));
        }
    }

    /// Commit one complete streamed line, tracking fenced code blocks in
    /// message text (the fence lines themselves commit as plain messages).
    fn commit_stream_line(&mut self, kind: TailKind, line: String) {
        match kind {
            TailKind::Thought => self.push(Line::Thought(line)),
            TailKind::Message => {
                let trimmed = line.trim();
                if trimmed.starts_with("```") {
                    match self.code_lang.take() {
                        Some(_) => {} // closing fence
                        None => {
                            // Opening fence: capture the info string.
                            self.code_lang =
                                Some(trimmed.trim_start_matches('`').trim().to_string());
                        }
                    }
                    self.push(Line::Message(line));
                } else if let Some(lang) = &self.code_lang {
                    let lang = lang.clone();
                    self.push(Line::Code { text: line, lang });
                } else {
                    self.push(Line::Message(line));
                }
            }
        }
    }

    /// Append a tool call's diff as rendered diff lines, once per distinct
    /// diff per tool id (updates repeating the same diff are dropped).
    pub(super) fn push_tool_diff(&mut self, id: &str, raw: &str) {
        if self.tool_diffs.get(id).is_some_and(|prev| prev == raw) {
            return;
        }
        self.tool_diffs.insert(id.to_string(), raw.to_string());
        match parse_rendered_diff(raw) {
            Some(data) => {
                self.flush_tail();
                for line in diff_lines(&data) {
                    self.push(line);
                }
            }
            // Unstructured diff content: show it as-is rather than dropping it.
            None => {
                self.flush_tail();
                for l in raw.lines() {
                    self.push(Line::Message(l.to_string()));
                }
            }
        }
    }

    /// Page the view up (into history), pinning it if it was following.
    pub(super) fn scroll_page_up(&mut self) {
        let page = self.viewport.max(1);
        let tail_start = self.lines.len().saturating_sub(page);
        let start_now = self.scroll.unwrap_or(tail_start);
        self.scroll = Some(start_now.saturating_sub(page));
    }

    /// Page the view down (toward the tail); reaching it resumes following.
    pub(super) fn scroll_page_down(&mut self) {
        let page = self.viewport.max(1);
        if let Some(s) = self.scroll {
            let next = s + page;
            if next + page >= self.lines.len() {
                self.scroll = None; // back at the tail — follow again
            } else {
                self.scroll = Some(next);
            }
        }
    }

    /// Actionability bucket for the roster sort. Lower = closer to the top.
    pub(super) fn bucket(&self) -> u8 {
        if self.pending.is_some() {
            0 // needs you
        } else if self.review.is_some() && !self.exited {
            1 // ready to review (the rail head's second tier)
        } else if self.attention {
            2 // something went wrong in the background
        } else if self.done && !self.exited {
            3 // finished, unseen — inbox material until viewed
        } else if !self.exited && self.turn_active {
            4 // working (a turn is in flight)
        } else if !self.exited {
            5 // idle
        } else {
            6 // dead
        }
    }

    /// Compact time-in-state for the rail row (`42s`, `7m`, `1h05m`) — shown
    /// for the states the human watches or acts on (needs-you, review,
    /// attention, done, working). Idle and dead rows stay calm.
    pub fn elapsed_label(&self, tick: u64) -> Option<String> {
        if self.exited {
            return None;
        }
        let watched = self.pending.is_some()
            || self.review.is_some()
            || self.attention
            || self.done
            || self.turn_active;
        if !watched {
            return None;
        }
        Some(fmt_elapsed(tick.saturating_sub(self.since) / TICKS_PER_SEC))
    }
}
