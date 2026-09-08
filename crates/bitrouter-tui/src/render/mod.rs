//! Turning journal entries into lines: one trait, one registry.
//!
//! # One registry, not two
//!
//! An earlier draft had a second registry for "router surfaces" — the cost
//! line and the provider picker. Neither could be expressed through it: the
//! cost line needs typed fields off `UsageUpdate`, and the picker is not a
//! session update at all but an out-of-band `_bitrouter/route/list` plus
//! keyboard input, which no render trait describes. A registry with no expressible
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
//! [`ToolContext`] includes command input and viewport size so commands can be
//! framed at the available width and diffs bounded by the terminal height.

pub mod content;
pub mod diff;
pub mod markdown;
pub mod session;

use std::borrow::Cow;
use std::collections::HashMap;

use agent_client_protocol_schema::v1::{
    ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolKind,
};
use ratatui::layout::Size;
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
    /// The terminal height, which is what bounds a single diff: one edit must
    /// never occupy more than the screen the rest of the session lives in.
    pub height: u16,
    /// Available transcript width, excluding the outer padding.
    pub width: u16,
    /// Structured tool arguments, including the command for execution calls.
    pub raw_input: Option<&'a serde_json::Value>,
}

impl<'a> ToolContext<'a> {
    /// Borrow a journal's tool call for rendering.
    ///
    /// One place knows which fields a renderer may see, so adding a field to
    /// the protocol does not quietly widen what renderers can reach.
    pub fn new(call: &'a ToolCall, size: Size) -> Self {
        Self {
            id: &call.tool_call_id,
            kind: call.kind,
            status: call.status,
            title: &call.title,
            content: &call.content,
            height: size.height,
            width: size.width,
            raw_input: call.raw_input.as_ref(),
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
    fn default() -> Self {
        let mut registry = Self {
            renderers: HashMap::new(),
            fallback: Box::new(Generic),
        };
        registry.register(ToolKey::Think, Box::new(Reasoning));
        registry.register(ToolKey::Execute, Box::new(Execute));
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
        if command_input(ctx).is_some() {
            return Execute.render(ctx);
        }
        let mut lines = vec![header(ctx)];
        lines.extend(content_lines(ctx));
        lines
    }
}

/// Execution calls show their literal shell command in a subdued code frame.
pub struct Execute;

impl ToolRenderer for Execute {
    fn render(&self, ctx: &ToolContext<'_>) -> Vec<Line<'static>> {
        let command = command_input(ctx).unwrap_or(Cow::Borrowed(ctx.title));
        let title_is_command = ctx.title == command
            || shell_words::split(ctx.title)
                .ok()
                .is_some_and(|words| words.len() == 1 && words[0] == command);
        let title = if title_is_command || ctx.title.is_empty() {
            "Command"
        } else {
            ctx.title
        };
        let mut lines = vec![titled_header(ctx.status, title)];
        if !command.is_empty() {
            lines.extend(markdown::code_block(&command, ctx.width));
        }
        lines.extend(content_lines(ctx));
        lines
    }
}

// ACP adapters may publish a shell script or argv, and may classify shell
// reads/searches by intent. Recognize structured command input for both.
fn command_input<'a>(ctx: &ToolContext<'a>) -> Option<Cow<'a, str>> {
    let input = ctx.raw_input?;
    let value = input.get("command").or_else(|| input.get("cmd"))?;
    if let Some(command) = value.as_str() {
        return Some(Cow::Borrowed(command));
    }
    let argv: Vec<&str> = value
        .as_array()?
        .iter()
        .map(|value| value.as_str())
        .collect::<Option<_>>()?;
    if let [shell, flag, script] = argv.as_slice()
        && matches!(
            shell.rsplit('/').next(),
            Some("sh" | "bash" | "zsh" | "fish" | "dash")
        )
        && matches!(*flag, "-c" | "-lc" | "-ic")
    {
        return Some(Cow::Borrowed(script));
    }
    (!argv.is_empty()).then(|| Cow::Owned(shell_words::join(argv)))
}

/// Reasoning tools use the same bright action style as thought messages.
pub struct Reasoning;

impl ToolRenderer for Reasoning {
    fn render(&self, ctx: &ToolContext<'_>) -> Vec<Line<'static>> {
        let mut lines = vec![header(ctx)];
        for item in ctx.content {
            match item {
                ToolCallContent::Content(block) => {
                    if let agent_client_protocol_schema::v1::ContentBlock::Text(text) =
                        &block.content
                    {
                        lines.extend(thought(&text.text, ctx.width));
                    } else {
                        lines.extend(content::render(&block.content));
                    }
                }
                ToolCallContent::Diff(file) => lines.extend(diff::render(file, ctx.height)),
                _ => {}
            }
        }
        lines
    }
}

/// One message run, in its own voice.
///
/// Not a registry entry: the registry keys on `ToolKind`, and a message has no
/// kind. There are exactly three voices and they are fixed by the protocol, so
/// a lookup table would be indirection with nothing to look up.
pub fn message(message: &crate::journal::Message, width: u16) -> Vec<Line<'static>> {
    match message.voice {
        crate::journal::Voice::User => {
            let mut lines = vec![Line::default()];
            lines.extend(message.text.split('\n').map(|line| {
                Line::from(Span::styled(
                    format!("> {line}"),
                    Style::default().fg(Color::Cyan),
                ))
            }));
            lines.push(Line::default());
            lines
        }
        crate::journal::Voice::Agent => markdown::render(&message.text, width),
        crate::journal::Voice::Thought => thought(&message.text, width),
    }
}

fn thought(text: &str, width: u16) -> Vec<Line<'static>> {
    markdown::render(text, width.saturating_sub(2).max(1))
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::styled("· ", thought_style())];
            spans.extend(
                line.spans
                    .into_iter()
                    .map(|span| Span::styled(span.content, span.style.patch(thought_style()))),
            );
            Line::from(spans)
        })
        .collect()
}

fn thought_style() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
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
    let title = if ctx.title.is_empty() {
        ctx.id.0.to_string()
    } else {
        ctx.title.to_string()
    };
    titled_header(ctx.status, &title)
}

fn titled_header(status: ToolCallStatus, title: &str) -> Line<'static> {
    let (glyph, color) = status_glyph(status);
    Line::from(vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::styled(title.to_string(), thought_style()),
    ])
}

/// Everything a call has produced, each piece through the renderer that knows
/// how to bound it.
fn content_lines(ctx: &ToolContext<'_>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for item in ctx.content {
        match item {
            ToolCallContent::Content(block) => lines.extend(content::render(&block.content)),
            ToolCallContent::Diff(file) => lines.extend(diff::render(file, ctx.height)),
            // Terminal handles are protocol metadata, not transcript content.
            ToolCallContent::Terminal(_) => {}
            // `ToolCallContent` is `#[non_exhaustive]`.
            _ => {}
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::{ContentBlock, Diff, TextContent};

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
            text(&registry.render(&ToolContext::new(&searched, Size::new(80, 24)))),
            "searched",
            "a registered key reaches its renderer"
        );

        // `Fetch` has no renderer registered, so this must reach `Generic` —
        // and `Generic` draws the title, which no spy does.
        let fetched = call(ToolKind::Fetch, "GET https://example.test");
        let rendered = text(&registry.render(&ToolContext::new(&fetched, Size::new(80, 24))));
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
            text(&registry.render(&ToolContext::new(&edit, Size::new(80, 24)))),
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

    /// A caller that registers nothing still gets *every* v1 kind drawn — the
    /// fallback covers each kind without a renderer of its own, so none of
    /// them renders blank.
    ///
    /// This replaced a test that asserted `Edit` and `Execute` were present in
    /// the registry's map. That assertion held while their renderers were
    /// byte-identical to the fallback, which is exactly how two renderers that
    /// drew nothing of their own survived: a test on a `HashMap` key cannot
    /// see what a renderer draws.
    #[test]
    fn every_v1_kind_renders_something() {
        let registry = Registry::default();
        for kind in [
            ToolKind::Read,
            ToolKind::Edit,
            ToolKind::Delete,
            ToolKind::Move,
            ToolKind::Search,
            ToolKind::Execute,
            ToolKind::Think,
            ToolKind::Fetch,
            ToolKind::SwitchMode,
        ] {
            let drawn = call(kind, "a title");
            let rendered = text(&registry.render(&ToolContext::new(&drawn, Size::new(80, 24))));
            assert!(
                rendered.contains("a title"),
                "{kind:?} rendered nothing: {rendered:?}"
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
        let rendered =
            text(&Registry::default().render(&ToolContext::new(&executed, Size::new(80, 24))));
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
        let rendered =
            text(&Registry::default().render(&ToolContext::new(&edited, Size::new(80, 24))));
        // The name, not the shape: the diff renderer absolutizes, and an
        // absolute path is `D:\...\src\lib.rs` on Windows.
        assert!(rendered.contains("lib.rs"), "{rendered:?}");
        assert!(rendered.contains("-let a = 1;"), "{rendered:?}");
        assert!(rendered.contains("+let b = 2;"), "{rendered:?}");
    }

    /// Reasoning tool content shares the bright style of action messages.
    #[test]
    fn a_think_call_is_drawn_in_the_reasoning_voice() {
        let thinking = call(ToolKind::Think, "considering the options").content(vec![
            ToolCallContent::Content(agent_client_protocol_schema::v1::Content::new(
                ContentBlock::Text(TextContent::new("weighing two designs")),
            )),
        ]);
        let lines = Registry::default().render(&ToolContext::new(&thinking, Size::new(80, 24)));
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
        let rendered =
            text(&Registry::default().render(&ToolContext::new(&untitled, Size::new(80, 24))));
        assert!(rendered.contains("t-42"), "{rendered:?}");
    }
    #[test]
    fn user_messages_keep_literal_text_and_surrounding_space() {
        let input = crate::journal::Message {
            voice: crate::journal::Voice::User,
            text: "Explain **this**\nand `that`".into(),
            complete: true,
        };
        assert_eq!(
            text(&message(&input, 80)),
            "\n> Explain **this**\n> and `that`\n"
        );
    }

    #[test]
    fn thought_captions_render_without_markdown_delimiters_in_bright_white() {
        let input = crate::journal::Message {
            voice: crate::journal::Voice::Thought,
            text: "**Inspecting root files**".into(),
            complete: false,
        };
        let lines = message(&input, 80);
        assert_eq!(text(&lines), "· Inspecting root files");
        assert!(lines.iter().flat_map(|line| &line.spans).all(|span| {
            span.style.fg == Some(Color::White)
                && span.style.add_modifier.contains(Modifier::BOLD)
                && !span.style.add_modifier.contains(Modifier::DIM)
        }));
    }

    #[test]
    fn shell_reads_render_commands_without_terminal_handles_or_losing_output() {
        let read = call(ToolKind::Read, "Read README.md")
            .raw_input(serde_json::json!({"command": ["/bin/zsh", "-lc", "cat README.md"]}))
            .content(vec![
                ToolCallContent::Terminal(agent_client_protocol_schema::v1::Terminal::new(
                    "exec-secret-id",
                )),
                ToolCallContent::Content(agent_client_protocol_schema::v1::Content::new(
                    ContentBlock::Text(TextContent::new("# raw output")),
                )),
            ]);
        let lines = Registry::default().render(&ToolContext::new(&read, Size::new(40, 24)));
        let output = text(&lines);
        assert!(output.contains("● Read README.md"), "{output}");
        assert!(output.contains("│ cat README.md"), "{output}");
        assert!(output.contains("# raw output"), "{output}");
        assert!(!output.contains("exec-secret-id"), "{output}");
        assert!(!output.contains("/bin/zsh"), "{output}");
        let command_spans = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.content.contains("cat README.md"));
        for span in command_spans {
            assert_eq!(span.style.fg, Some(Color::DarkGray));
            assert!(!span.style.add_modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn argv_quoting_and_literal_execute_titles_survive() {
        let executed = call(ToolKind::Execute, "Run checks")
            .raw_input(serde_json::json!({"command": ["printf", "%s", "two words"]}));
        let output =
            text(&Registry::default().render(&ToolContext::new(&executed, Size::new(80, 24))));
        assert!(output.contains("printf '%s' 'two words'"), "{output}");
        let fallback = call(ToolKind::Execute, "echo '**literal**'");
        let output =
            text(&Registry::default().render(&ToolContext::new(&fallback, Size::new(80, 24))));
        assert!(output.starts_with("● Command\n┌"), "{output}");
        assert!(output.contains("│ echo '**literal**'"), "{output}");
    }
}
