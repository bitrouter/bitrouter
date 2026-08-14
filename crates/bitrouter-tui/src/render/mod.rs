//! Turning journal entries into lines: one trait, one registry.
//!
//! # One registry, not two
//!
//! An earlier draft had a second registry for "router surfaces" — the cost
//! line and the provider picker. Neither could be expressed through it: the
//! cost line needs typed fields off `UsageUpdate`, and the picker is not a
//! session update at all but an out-of-band `providers/list` plus keyboard
//! input, which no render trait describes. A registry with no expressible
//! entries is dead weight, so there is exactly one, over tool calls, and the
//! footer is composed by the caller instead.
//!
//! # Why the key is not `ToolKind`
//!
//! `ToolKind` derives `PartialEq, Eq` and neither `Hash` nor `Ord`, and the
//! orphan rule stops us adding them. [`ToolKey`] mirrors it so a `HashMap` is
//! possible at all — and gives the `#[non_exhaustive]` wildcard a defined
//! landing spot, so an unknown future kind renders as `Other` rather than
//! failing to compile or being silently dropped.
//!
//! # What a renderer is given
//!
//! [`ToolContext`] carries the id, kind, status, title, content, and width —
//! and nothing else. No `expanded` (expand/collapse is deferred), no
//! `raw_input`, no `locations`: no renderer here reads them, and a field
//! added before its reader exists is dead code.

use std::collections::HashMap;

use agent_client_protocol_schema::v1::{
    ContentBlock, ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolKind,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Everything a tool-call renderer is allowed to see.
pub struct ToolContext<'a> {
    /// The call's id, for renderers that need to name it.
    pub id: &'a ToolCallId,
    /// What kind of work the call is doing.
    pub kind: ToolKind,
    /// How far along it is.
    pub status: ToolCallStatus,
    /// The agent's own one-line description.
    pub title: &'a str,
    /// Everything the call has produced so far.
    pub content: &'a [ToolCallContent],
    /// The terminal width the result will be wrapped to.
    pub width: u16,
}

impl<'a> ToolContext<'a> {
    /// Borrow a journal's tool call for rendering.
    ///
    /// One place knows which fields a renderer may see, so adding a field to
    /// the protocol does not quietly widen what renderers can reach.
    pub fn new(call: &'a ToolCall, width: u16) -> Self {
        Self {
            id: &call.tool_call_id,
            kind: call.kind,
            status: call.status,
            title: &call.title,
            content: &call.content,
            width,
        }
    }
}

/// How one kind of tool call is drawn.
pub trait ToolRenderer {
    /// The rows this call occupies, unwrapped — wrapping happens later, over
    /// the finished document (see [`crate::wrap`]).
    fn render(&self, ctx: &ToolContext<'_>) -> Vec<Line<'static>>;
}

/// Registry key: a mirror of `ToolKind` that can be hashed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolKey {
    /// Reading files or data.
    Read,
    /// Modifying files or content.
    Edit,
    /// Removing files or data.
    Delete,
    /// Moving or renaming files.
    Move,
    /// Searching for information.
    Search,
    /// Running commands or code.
    Execute,
    /// Internal reasoning or planning.
    Think,
    /// Retrieving external data.
    Fetch,
    /// Switching the current session mode.
    SwitchMode,
    /// Anything else, including kinds added to the protocol after this build.
    Other,
}

impl From<ToolKind> for ToolKey {
    fn from(kind: ToolKind) -> Self {
        match kind {
            ToolKind::Read => Self::Read,
            ToolKind::Edit => Self::Edit,
            ToolKind::Delete => Self::Delete,
            ToolKind::Move => Self::Move,
            ToolKind::Search => Self::Search,
            ToolKind::Execute => Self::Execute,
            ToolKind::Think => Self::Think,
            ToolKind::Fetch => Self::Fetch,
            ToolKind::SwitchMode => Self::SwitchMode,
            // `ToolKind` is `#[non_exhaustive]`: a kind this build has never
            // heard of lands here rather than anywhere undefined.
            _ => Self::Other,
        }
    }
}

/// Which renderer draws which kind of call.
///
/// Everything unregistered falls through to [`Generic`], so an agent using a
/// kind nobody wrote a renderer for still gets a title, a status, and its
/// content — never a blank.
pub struct Registry {
    renderers: HashMap<ToolKey, Box<dyn ToolRenderer>>,
    fallback: Box<dyn ToolRenderer>,
}

impl Default for Registry {
    /// The v1 set: diffs for edits, captured output for commands, reasoning
    /// for thinking, and [`Generic`] for everything else.
    fn default() -> Self {
        let mut registry = Self {
            renderers: HashMap::new(),
            fallback: Box::new(Generic),
        };
        registry.register(ToolKey::Edit, Box::new(Diffs));
        registry.register(ToolKey::Execute, Box::new(Output));
        registry.register(ToolKey::Think, Box::new(Reasoning));
        registry
    }
}

impl Registry {
    /// Point a key at a renderer, replacing any previous one.
    pub fn register(&mut self, key: ToolKey, renderer: Box<dyn ToolRenderer>) {
        self.renderers.insert(key, renderer);
    }

    /// Draw a call through whichever renderer owns its kind.
    pub fn render(&self, ctx: &ToolContext<'_>) -> Vec<Line<'static>> {
        match self.renderers.get(&ToolKey::from(ctx.kind)) {
            Some(renderer) => renderer.render(ctx),
            None => self.fallback.render(ctx),
        }
    }
}

/// The default renderer: a header, then whatever the call has produced.
pub struct Generic;

impl ToolRenderer for Generic {
    fn render(&self, ctx: &ToolContext<'_>) -> Vec<Line<'static>> {
        let mut lines = vec![header(ctx)];
        lines.extend(content_lines(ctx.content));
        lines
    }
}

/// `Edit` calls, whose content is what changed on disk.
pub struct Diffs;

impl ToolRenderer for Diffs {
    fn render(&self, ctx: &ToolContext<'_>) -> Vec<Line<'static>> {
        let mut lines = vec![header(ctx)];
        lines.extend(content_lines(ctx.content));
        lines
    }
}

/// `Execute` calls, whose content is what the command printed.
pub struct Output;

impl ToolRenderer for Output {
    fn render(&self, ctx: &ToolContext<'_>) -> Vec<Line<'static>> {
        let mut lines = vec![header(ctx)];
        lines.extend(content_lines(ctx.content));
        lines
    }
}

/// `Think` calls — the agent reasoning through a tool rather than in a thought
/// chunk. Drawn in the same dimmed voice, because it is the same voice.
pub struct Reasoning;

impl ToolRenderer for Reasoning {
    fn render(&self, ctx: &ToolContext<'_>) -> Vec<Line<'static>> {
        let mut lines = vec![header(ctx)];
        for line in content_lines(ctx.content) {
            lines.push(Line::from(
                line.spans
                    .into_iter()
                    .map(|span| {
                        let content = span.content.into_owned();
                        Span::styled(content, thought_style())
                    })
                    .collect::<Vec<_>>(),
            ));
        }
        lines
    }
}

/// The dimmed italic the agent's reasoning is drawn in, wherever it appears.
fn thought_style() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}

/// What a tool call is doing, as one glyph.
///
/// A pending call and a failed one must never look alike: the whole reason to
/// render status is so a stalled turn is distinguishable from a broken one.
fn status_glyph(status: ToolCallStatus) -> (&'static str, Color) {
    match status {
        ToolCallStatus::Pending => ("◌", Color::DarkGray),
        ToolCallStatus::InProgress => ("◍", Color::Yellow),
        ToolCallStatus::Completed => ("●", Color::Green),
        ToolCallStatus::Failed => ("✗", Color::Red),
        // An unknown future status is reported as unknown, not quietly shown
        // as one of the four we do understand.
        _ => ("?", Color::Magenta),
    }
}

/// The call's first row: status, then the agent's own title.
///
/// A call with no title is named by its id rather than left blank — an
/// unlabelled row is still traceable, an empty one is not.
fn header(ctx: &ToolContext<'_>) -> Line<'static> {
    let (glyph, color) = status_glyph(ctx.status);
    let title = if ctx.title.is_empty() {
        ctx.id.0.to_string()
    } else {
        ctx.title.to_string()
    };
    Line::from(vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::raw(title),
    ])
}

/// A call's content, rendered plainly.
///
/// Deliberately simple: capping output, hunking diffs, and describing non-text
/// blocks are task 2.4's subject. What matters here is that content reaches
/// the screen at all — `Transcript` drops every variant but `Diff`, which is
/// defect 2.
fn content_lines(content: &[ToolCallContent]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for item in content {
        match item {
            ToolCallContent::Content(block) => {
                for text in block_text(&block.content).lines() {
                    lines.push(Line::from(format!("  {text}")));
                }
            }
            ToolCallContent::Diff(diff) => {
                lines.push(Line::from(Span::styled(
                    format!("  {}", diff.path.display()),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                if let Some(old) = &diff.old_text {
                    for removed in old.lines() {
                        lines.push(Line::from(Span::styled(
                            format!("  -{removed}"),
                            Style::default().fg(Color::Red),
                        )));
                    }
                }
                for added in diff.new_text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  +{added}"),
                        Style::default().fg(Color::Green),
                    )));
                }
            }
            ToolCallContent::Terminal(terminal) => {
                lines.push(Line::from(format!("  [terminal {}]", terminal.terminal_id)));
            }
            // `ToolCallContent` is `#[non_exhaustive]`.
            _ => {}
        }
    }
    lines
}

/// The text of a content block, or empty for the kinds that have none.
fn block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::{Diff, TextContent};

    use super::*;

    fn call(kind: ToolKind, title: &str) -> ToolCall {
        ToolCall::new(ToolCallId::new("t1"), title.to_string())
            .kind(kind)
            .status(ToolCallStatus::Completed)
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A renderer that says only its own name, so dispatch is observable
    /// without depending on what any real renderer draws.
    struct Spy(&'static str);

    impl ToolRenderer for Spy {
        fn render(&self, _ctx: &ToolContext<'_>) -> Vec<Line<'static>> {
            vec![Line::from(self.0)]
        }
    }

    /// The registry's whole job, both halves of it: a registered key reaches
    /// its renderer, and an unregistered one reaches the default.
    #[test]
    fn a_registered_key_dispatches_and_the_rest_fall_through() {
        let mut registry = Registry::default();
        registry.register(ToolKey::Search, Box::new(Spy("searched")));
        registry.register(ToolKey::Other, Box::new(Spy("fallback-was-not-used")));

        let searched = call(ToolKind::Search, "grep for it");
        assert_eq!(
            text(&registry.render(&ToolContext::new(&searched, 80))),
            "searched",
            "a registered key reaches its renderer"
        );

        // `Fetch` has no renderer registered, so this must reach `Generic` —
        // and `Generic` draws the title, which no spy does.
        let fetched = call(ToolKind::Fetch, "GET https://example.test");
        let rendered = text(&registry.render(&ToolContext::new(&fetched, 80)));
        assert!(
            rendered.contains("GET https://example.test"),
            "an unregistered key falls through to the default: {rendered:?}"
        );
        assert!(
            !rendered.contains("fallback-was-not-used"),
            "`Other` is a key like any other, not the fallback: {rendered:?}"
        );
    }

    /// Registering over a key replaces the renderer rather than adding a
    /// second one nobody can reach.
    #[test]
    fn registering_twice_replaces() {
        let mut registry = Registry::default();
        registry.register(ToolKey::Edit, Box::new(Spy("first")));
        registry.register(ToolKey::Edit, Box::new(Spy("second")));
        let edit = call(ToolKind::Edit, "Edit src/lib.rs");
        assert_eq!(
            text(&registry.render(&ToolContext::new(&edit, 80))),
            "second"
        );
    }

    /// Every `ToolKind` maps to exactly one key, and the wildcard has a
    /// defined landing spot instead of being a compile error waiting to
    /// happen.
    #[test]
    fn every_kind_has_a_key() {
        let pairs = [
            (ToolKind::Read, ToolKey::Read),
            (ToolKind::Edit, ToolKey::Edit),
            (ToolKind::Delete, ToolKey::Delete),
            (ToolKind::Move, ToolKey::Move),
            (ToolKind::Search, ToolKey::Search),
            (ToolKind::Execute, ToolKey::Execute),
            (ToolKind::Think, ToolKey::Think),
            (ToolKind::Fetch, ToolKey::Fetch),
            (ToolKind::SwitchMode, ToolKey::SwitchMode),
            (ToolKind::Other, ToolKey::Other),
        ];
        for (kind, key) in pairs {
            assert_eq!(ToolKey::from(kind), key);
        }
        let keys: std::collections::HashSet<ToolKey> =
            pairs.into_iter().map(|(_, key)| key).collect();
        assert_eq!(keys.len(), 10, "no two kinds share a key");
    }

    /// The v1 set is registered out of the box, so a caller that does nothing
    /// still gets diffs, output, and reasoning drawn by their own renderers.
    #[test]
    fn the_default_registry_covers_the_v1_kinds() {
        let registry = Registry::default();
        for kind in [ToolKind::Edit, ToolKind::Execute, ToolKind::Think] {
            assert!(
                registry.renderers.contains_key(&ToolKey::from(kind)),
                "{kind:?} must have a renderer of its own"
            );
        }
    }

    /// Defect 2, at the registry level: an `Execute` call's output has to
    /// reach the screen. `Transcript` drops every content variant but `Diff`.
    #[test]
    fn a_command_s_output_is_rendered_not_dropped() {
        let executed =
            call(ToolKind::Execute, "cargo test").content(vec![ToolCallContent::Content(
                agent_client_protocol_schema::v1::Content::new(ContentBlock::Text(
                    TextContent::new("running 3 tests\ntest result: ok."),
                )),
            )]);
        let rendered = text(&Registry::default().render(&ToolContext::new(&executed, 80)));
        assert!(rendered.contains("cargo test"), "{rendered:?}");
        assert!(rendered.contains("running 3 tests"), "{rendered:?}");
        assert!(rendered.contains("test result: ok."), "{rendered:?}");
    }

    /// An edit still names the file it changed and shows both sides.
    #[test]
    fn an_edit_names_its_file_and_both_sides() {
        let edited = call(ToolKind::Edit, "Edit src/lib.rs").content(vec![ToolCallContent::Diff(
            Diff::new("src/lib.rs", "let b = 2;").old_text("let a = 1;".to_string()),
        )]);
        let rendered = text(&Registry::default().render(&ToolContext::new(&edited, 80)));
        assert!(rendered.contains("src/lib.rs"), "{rendered:?}");
        assert!(rendered.contains("-let a = 1;"), "{rendered:?}");
        assert!(rendered.contains("+let b = 2;"), "{rendered:?}");
    }

    /// Reasoning is drawn as reasoning: the same dimmed italic a thought
    /// chunk gets, so the two cannot be confused with the agent's answer.
    #[test]
    fn a_think_call_is_drawn_in_the_reasoning_voice() {
        let thinking = call(ToolKind::Think, "considering the options").content(vec![
            ToolCallContent::Content(agent_client_protocol_schema::v1::Content::new(
                ContentBlock::Text(TextContent::new("weighing two designs")),
            )),
        ]);
        let lines = Registry::default().render(&ToolContext::new(&thinking, 80));
        let styles: Vec<Style> = lines
            .iter()
            .skip(1)
            .flat_map(|line| line.spans.iter().map(|span| span.style))
            .collect();
        assert!(!styles.is_empty(), "the content must render at all");
        for style in styles {
            assert_eq!(style, thought_style());
        }
    }

    /// Status has to be visible and unambiguous — a stalled call must not look
    /// like a broken one.
    #[test]
    fn every_status_is_distinguishable() {
        let glyphs: Vec<&str> = [
            ToolCallStatus::Pending,
            ToolCallStatus::InProgress,
            ToolCallStatus::Completed,
            ToolCallStatus::Failed,
        ]
        .into_iter()
        .map(|status| status_glyph(status).0)
        .collect();
        let unique: std::collections::BTreeSet<&str> = glyphs.iter().copied().collect();
        assert_eq!(unique.len(), glyphs.len(), "{glyphs:?}");
    }

    /// A call the agent never titled is named by its id. A blank row would be
    /// untraceable.
    #[test]
    fn an_untitled_call_is_named_by_its_id() {
        let untitled = ToolCall::new(ToolCallId::new("t-42"), String::new());
        let rendered = text(&Registry::default().render(&ToolContext::new(&untitled, 80)));
        assert!(rendered.contains("t-42"), "{rendered:?}");
    }
}
