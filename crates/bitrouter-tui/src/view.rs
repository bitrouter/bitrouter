//! The live view of one ACP session: the document, and the rows below it.
//!
//! The document is the [`Journal`]; this owns the two things the journal
//! cannot know about — the footer composed around it, and the terminal the
//! writer paints into.
//!
//! # Cost is read, never inferred
//!
//! ACP carries `UsageUpdate.cost` but nothing saying *who wrote it*, and a
//! currency figure nobody can vouch for is the most misleading thing a
//! session view can draw. [`crate::cost::from_usage`] reads the provenance
//! marker off the `_meta` key the controller writes: marked `router`, the
//! figure is BitRouter's meter and is drawn plainly; unmarked, it is the
//! harness's own and is drawn labelled as the agent's; marked with something
//! this renderer does not know, it is not drawn at all.
//!
//! `None` is not an error and not a zero: it renders as *unreported*, which is
//! what a client that cannot see a price has actually observed.
//!
//! # Context occupancy is the half that needs no attribution
//!
//! The same `UsageUpdate` carries `used` and `size`, and those are the
//! harness's own: it owns the context window, and the figure means the same
//! thing whoever routed the traffic. So [`crate::render::session::context`]
//! draws them whenever the harness sends them, including in every session
//! where cost is *unreported*. It is drawn only as a pair — see that
//! function for why half of it is worse than none.
//!
//! # Ordering is part of the honesty
//!
//! The footer's rows go cost, context window, route, session state, notice,
//! pending permission, modal, input. Two of those placements are load-bearing rather
//! than aesthetic: a question waiting on a person outranks everything else
//! down here, and the input line is last so it is never pushed off-screen by
//! something the agent said.

use std::io;
use std::sync::{Mutex, MutexGuard};

use ratatui::text::{Line, Span};

use crate::journal::Journal;
use crate::render::Registry;
use crate::writer::{Cache, Writer};

/// The live view of one chat session.
pub struct View {
    writer: Writer<ratatui::backend::CrosstermBackend<io::Stdout>>,
    cache: Cache,
    registry: Registry,
    /// How this session is routed. Fixed for its life unless the caller
    /// changes it, and absent when the session is direct.
    route: Option<String>,
    /// The most recent thing the client itself has to say — a stop reason, a
    /// route change, a failed turn, the agent's command list. Replaced rather
    /// than accumulated: it is about the last thing that happened, and the
    /// session log is where the history lives.
    notice: Vec<Line<'static>>,
    /// A row owned by an open modal. Today only the provider picker.
    modal: Option<Line<'static>>,
    /// The line being typed. Raw mode means nothing is echoed unless the
    /// footer echoes it.
    input: String,
}

impl View {
    /// Take the terminal.
    ///
    /// Call this **before** the caller's stdin owner exists: the writer reads
    /// the cursor once at construction, which on a real terminal is a DSR
    /// query, and a reader already sitting on stdin would take the answer.
    pub fn open(route: Option<String>) -> io::Result<Self> {
        let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
        Ok(Self {
            writer: Writer::new(backend)?,
            cache: Cache::default(),
            registry: Registry::default(),
            route,
            notice: Vec::new(),
            modal: None,
            input: String::new(),
        })
    }

    /// Replace the line being echoed. An empty string clears it.
    pub fn set_input(&mut self, typed: &str) {
        self.input.clear();
        self.input.push_str(typed);
    }

    /// Change where this session reports it is routed.
    pub fn set_route(&mut self, route: Option<String>) {
        self.route = route;
    }

    /// Say one thing, replacing whatever was said last.
    pub fn notice(&mut self, text: impl Into<String>) {
        self.notice = vec![Line::from(text.into())];
    }

    /// Say something already rendered — the agent's command list, for one.
    pub fn notice_lines(&mut self, lines: Vec<Line<'static>>) {
        self.notice = lines;
    }

    /// Stop saying anything.
    pub fn clear_notice(&mut self) {
        self.notice.clear();
    }

    /// Give a modal its row. Today only the provider picker has one.
    pub fn open_modal(&mut self, row: Line<'static>) {
        self.modal = Some(row);
    }

    /// Take the modal's row back.
    pub fn close_modal(&mut self) {
        self.modal = None;
    }

    /// The rows below the document: what this session costs and where it is
    /// going, then whatever is currently being asked or typed.
    ///
    /// Re-emitted as the document's tail on every frame, so it is always the
    /// newest content and always in the live region — which is what bounds the
    /// stale-row cost to individual scrolled-off tool calls rather than to the
    /// session's own summary.
    fn footer(&self, journal: &Journal) -> Vec<Line<'static>> {
        let cost = journal
            .usage()
            .and_then(crate::cost::from_usage)
            .map_or_else(crate::cost::unreported, |cost| cost.render());
        let mut status: Vec<Span<'static>> = cost.spans;
        // Beside the cost, because both answer "what is this turn consuming" —
        // and this half needs no attribution, so it shows where cost cannot.
        status.extend(crate::render::session::context(journal.usage()));
        if let Some(route) = &self.route {
            status.push(Span::raw(format!(" · via {route}")));
        }
        status.extend(crate::render::session::state(
            journal.mode(),
            journal.config(),
            journal.title(),
        ));

        let mut rows = vec![Line::from(status)];
        rows.extend(self.notice.iter().cloned());
        // A question waiting on a person outranks anything else down here.
        if let Some(prompt) = journal.pending_permission() {
            rows.push(prompt.render());
        }
        if let Some(modal) = &self.modal {
            rows.push(modal.clone());
        }
        rows.push(Line::from(format!("> {}", self.input)));
        rows
    }

    /// Paint one frame.
    ///
    /// Every caller of this has already decided the frame is worth painting —
    /// the writer still writes nothing when the document is unchanged, so a
    /// redundant call costs a render and no output.
    pub fn paint(&mut self, shared: &Mutex<Journal>) -> io::Result<()> {
        let size = self.writer.size();
        let journal = lock(shared);
        let footer = self.footer(&journal);
        let document = self.cache.document(&journal, &self.registry, size, &footer);
        // The lock is not held across the write: a frame can take a
        // millisecond, and the caller's pump has updates to apply.
        drop(journal);
        self.writer.frame(&document)
    }

    /// Repaint everything on screen, whatever the writer believes is there.
    ///
    /// For when something else has written to this terminal: the writer paints
    /// against its own model and never asks the terminal what it holds, so a
    /// stray line leaves every row below it one off. `Ctrl-L` is how a person
    /// says so.
    pub fn redraw(&mut self, shared: &Mutex<Journal>) -> io::Result<()> {
        self.writer.invalidate();
        self.paint(shared)
    }

    /// Leave the cursor below the document. Nothing rendered is removed.
    pub fn finish(&mut self) -> io::Result<()> {
        self.writer.finish()
    }
}

/// Read the journal, whatever the last holder of the lock did.
///
/// A poisoned lock costs a frame at most: the journal is plain data, a panic
/// mid-`apply` leaves it merely stale, and refusing to draw at all would turn
/// one lost update into a dead session.
pub fn lock(shared: &Mutex<Journal>) -> MutexGuard<'_, Journal> {
    match shared.lock() {
        Ok(journal) => journal,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::UsageUpdate;

    fn usage(cost: bool, marker: Option<&str>) -> UsageUpdate {
        let mut usage = UsageUpdate::new(1_500, 200_000);
        if cost {
            usage.cost = Some(agent_client_protocol_schema::v1::Cost::new(0.42, "USD"));
        }
        if let Some(marker) = marker {
            let mut meta = serde_json::Map::new();
            meta.insert(
                crate::cost::COST_PROVENANCE_META_KEY.to_string(),
                serde_json::Value::String(marker.to_string()),
            );
            usage.meta = Some(meta);
        }
        usage
    }

    fn rendered(usage: &UsageUpdate) -> String {
        crate::plain::text(
            &crate::cost::from_usage(usage).map_or_else(crate::cost::unreported, |c| c.render()),
        )
    }

    /// No figure at all renders *unreported*, never a zero. This is the case
    /// every harness that does not report cost lands in — and every
    /// unmetered BitRouter session, which is forwarded with no figure.
    #[test]
    fn an_absent_cost_renders_unreported() {
        let text = rendered(&usage(false, None));
        assert!(text.contains("unreported"), "{text:?}");
        assert!(!text.contains('0'), "{text:?}");
    }

    /// The marker on the wire is what reaches the line — the view never
    /// decides whose number it is.
    #[test]
    fn the_wire_provenance_is_the_one_rendered() {
        let ours = rendered(&usage(true, Some(crate::cost::COST_PROVENANCE_ROUTER)));
        assert!(ours.contains("0.42"), "{ours:?}");
        assert!(!ours.contains("agent"), "{ours:?}");

        let theirs = rendered(&usage(true, None));
        assert!(theirs.contains("agent"), "{theirs:?}");
        assert!(theirs.contains("0.42"), "{theirs:?}");

        let unknown = rendered(&usage(true, Some("something-new")));
        assert!(unknown.contains("unreported"), "{unknown:?}");
    }
}
