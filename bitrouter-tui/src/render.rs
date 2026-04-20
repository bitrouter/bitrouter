use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Character-preserving hard-break at exactly `width` characters.
///
/// Preserves ALL whitespace verbatim (leading spaces, tabs, everything).
/// If `width == 0` or the line fits within `width`, returns `vec![line.to_string()]`.
/// An empty line returns `vec![String::new()]`.
pub(crate) fn hard_wrap(line: &str, width: usize) -> Vec<String> {
    if width == 0 || line.chars().count() <= width {
        return vec![line.to_string()];
    }

    let mut result = Vec::new();
    let mut chars = line.chars();
    loop {
        let chunk: String = chars.by_ref().take(width).collect();
        if chunk.is_empty() {
            break;
        }
        result.push(chunk);
    }

    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// Word-boundary-aware wrapping that preserves leading whitespace.
///
/// Scans characters and tracks position. When reaching `width`, scans backward
/// for the last whitespace to break at. If no whitespace is found (e.g., a long
/// URL), falls back to a hard-cut at `width`.
pub(crate) fn prose_wrap(line: &str, width: usize) -> Vec<String> {
    if width == 0 || line.chars().count() <= width {
        return vec![line.to_string()];
    }

    let mut result = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut start = 0;

    while start < len {
        // Remaining fits in one line.
        if start + width >= len {
            result.push(chars[start..len].iter().collect());
            break;
        }

        // Look for the last whitespace within the width window.
        let end = start + width;
        let mut break_at = None;
        for i in (start..end).rev() {
            if chars[i].is_whitespace() {
                break_at = Some(i);
                break;
            }
        }

        match break_at {
            Some(pos) => {
                // Include everything up to (but not including) the whitespace.
                result.push(chars[start..pos].iter().collect());
                // Skip the whitespace character itself.
                start = pos + 1;
            }
            None => {
                // No whitespace found — hard-cut at width.
                result.push(chars[start..end].iter().collect());
                start = end;
            }
        }
    }

    if result.is_empty() {
        result.push(String::new());
    }
    result
}

// ── Markdown rendering ─────────────────────────────────────────────────

/// State machine for fenced code blocks.
enum MdState {
    Normal,
    CodeFence,
}

/// Render markdown-flavored text into styled `Line`s, calling `gutter()`
/// once per output line to produce the leading span.
pub(crate) fn render_markdown(
    text: &str,
    width: usize,
    gutter: impl Fn() -> Span<'static>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut state = MdState::Normal;

    for raw_line in text.lines() {
        match state {
            MdState::Normal => {
                let trimmed = raw_line.trim_start();

                // ── Code fence open ───────────────────────────────
                if trimmed.starts_with("```") {
                    state = MdState::CodeFence;
                    let lang = trimmed.trim_start_matches('`').trim();
                    let label = if lang.is_empty() {
                        "```".to_string()
                    } else {
                        format!("``` {lang}")
                    };
                    lines.push(Line::from(vec![
                        gutter(),
                        Span::styled(label, Style::default().fg(Color::DarkGray)),
                    ]));
                    continue;
                }

                // ── Headers ───────────────────────────────────────
                if let Some(rest) = trimmed.strip_prefix("### ") {
                    for seg in hard_wrap(rest, width) {
                        lines.push(Line::from(vec![
                            gutter(),
                            Span::styled(
                                seg,
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]));
                    }
                    continue;
                }
                if let Some(rest) = trimmed.strip_prefix("## ") {
                    for seg in hard_wrap(rest, width) {
                        lines.push(Line::from(vec![
                            gutter(),
                            Span::styled(
                                seg,
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]));
                    }
                    continue;
                }
                if let Some(rest) = trimmed.strip_prefix("# ") {
                    for seg in hard_wrap(rest, width) {
                        lines.push(Line::from(vec![
                            gutter(),
                            Span::styled(
                                seg,
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]));
                    }
                    continue;
                }

                // ── Horizontal rule ───────────────────────────────
                if is_horizontal_rule(trimmed) {
                    let rule: String = "─".repeat(width);
                    lines.push(Line::from(vec![
                        gutter(),
                        Span::styled(rule, Style::default().fg(Color::DarkGray)),
                    ]));
                    continue;
                }

                // ── Unordered list ────────────────────────────────
                if let Some(captures) = strip_unordered_list(raw_line) {
                    let (indent, content) = captures;
                    let bullet_prefix = format!("{indent}• ");
                    let item_width = width.saturating_sub(bullet_prefix.chars().count());
                    let wrapped = prose_wrap(content, item_width);
                    for (i, seg) in wrapped.into_iter().enumerate() {
                        let mut spans = vec![gutter()];
                        if i == 0 {
                            spans.push(Span::raw(bullet_prefix.clone()));
                        } else {
                            // Continuation lines: align with the content after the bullet.
                            let pad: String = " ".repeat(bullet_prefix.chars().count());
                            spans.push(Span::raw(pad));
                        }
                        spans.extend(parse_inline(&seg));
                        lines.push(Line::from(spans));
                    }
                    continue;
                }

                // ── Ordered list ──────────────────────────────────
                if let Some(captures) = strip_ordered_list(raw_line) {
                    let (indent, number, content) = captures;
                    let num_prefix = format!("{indent}{number}. ");
                    let item_width = width.saturating_sub(num_prefix.chars().count());
                    let wrapped = prose_wrap(content, item_width);
                    for (i, seg) in wrapped.into_iter().enumerate() {
                        let mut spans = vec![gutter()];
                        if i == 0 {
                            spans.push(Span::raw(num_prefix.clone()));
                        } else {
                            let pad: String = " ".repeat(num_prefix.chars().count());
                            spans.push(Span::raw(pad));
                        }
                        spans.extend(parse_inline(&seg));
                        lines.push(Line::from(spans));
                    }
                    continue;
                }

                // ── Prose paragraph ───────────────────────────────
                let wrapped = prose_wrap(raw_line, width);
                for seg in wrapped {
                    let mut spans = vec![gutter()];
                    spans.extend(parse_inline(&seg));
                    lines.push(Line::from(spans));
                }
            }
            MdState::CodeFence => {
                let trimmed = raw_line.trim();
                // Closing fence: line is only backticks (3+) and optional whitespace.
                if trimmed.starts_with("```") && trimmed.trim_start_matches('`').trim().is_empty() {
                    state = MdState::Normal;
                    lines.push(Line::from(vec![
                        gutter(),
                        Span::styled("```".to_string(), Style::default().fg(Color::DarkGray)),
                    ]));
                    continue;
                }

                // Code line — hard wrap, DIM white.
                let code_style = Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::DIM);
                for seg in hard_wrap(raw_line, width) {
                    lines.push(Line::from(vec![gutter(), Span::styled(seg, code_style)]));
                }
            }
        }
    }

    // Handle trailing newline: `"foo\n".lines()` yields `["foo"]`, but the
    // source logically ends with an empty line. Append one if text ends with '\n'.
    if text.ends_with('\n') {
        lines.push(Line::from(vec![gutter()]));
    }

    if lines.is_empty() {
        lines.push(Line::from(vec![gutter()]));
    }

    lines
}

/// Returns `true` when the trimmed line is a horizontal rule (e.g. `---`, `***`, `___`).
fn is_horizontal_rule(trimmed: &str) -> bool {
    if trimmed.is_empty() {
        return false;
    }
    let without_spaces: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if without_spaces.len() < 3 {
        return false;
    }
    let first = without_spaces.chars().next();
    match first {
        Some('-') | Some('*') | Some('_') => without_spaces.chars().all(|c| Some(c) == first),
        _ => false,
    }
}

/// Try to parse an unordered list item. Returns `(indent, content)`.
fn strip_unordered_list(line: &str) -> Option<(&str, &str)> {
    let stripped = line.trim_start();
    let indent_len = line.len() - stripped.len();
    let indent = &line[..indent_len];
    let rest = if let Some(r) = stripped.strip_prefix("- ") {
        r
    } else { stripped.strip_prefix("* ")? };
    Some((indent, rest))
}

/// Try to parse an ordered list item. Returns `(indent, number_str, content)`.
fn strip_ordered_list(line: &str) -> Option<(&str, &str, &str)> {
    let stripped = line.trim_start();
    let indent_len = line.len() - stripped.len();
    let indent = &line[..indent_len];
    // Find digits at start.
    let digit_end = stripped
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(stripped.len());
    if digit_end == 0 {
        return None;
    }
    let after_digits = &stripped[digit_end..];
    if let Some(content) = after_digits.strip_prefix(". ") {
        let number = &stripped[..digit_end];
        Some((indent, number, content))
    } else {
        None
    }
}

/// Single-pass inline markup scanner.
///
/// Handles: `` ` `` (inline code), `**` (bold), `*` (italic).
fn parse_inline(s: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;

    let mut in_code = false;
    let mut in_bold = false;
    let mut in_italic = false;

    while i < len {
        let ch = chars[i];

        // Backtick: toggle inline code.
        if ch == '`' {
            if !buf.is_empty() {
                spans.push(make_inline_span(&buf, in_code, in_bold, in_italic));
                buf.clear();
            }
            in_code = !in_code;
            i += 1;
            continue;
        }

        // Inside code — no further markup parsing.
        if in_code {
            buf.push(ch);
            i += 1;
            continue;
        }

        // `**` bold toggle.
        if ch == '*' && i + 1 < len && chars[i + 1] == '*' {
            if !buf.is_empty() {
                spans.push(make_inline_span(&buf, false, in_bold, in_italic));
                buf.clear();
            }
            in_bold = !in_bold;
            i += 2;
            continue;
        }

        // `*` italic toggle (single star, not followed by another star).
        if ch == '*' {
            if !buf.is_empty() {
                spans.push(make_inline_span(&buf, false, in_bold, in_italic));
                buf.clear();
            }
            in_italic = !in_italic;
            i += 1;
            continue;
        }

        buf.push(ch);
        i += 1;
    }

    // Flush remaining buffer (unclosed markers stay active — streaming safe).
    if !buf.is_empty() {
        spans.push(make_inline_span(&buf, in_code, in_bold, in_italic));
    }

    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }

    spans
}

/// Build a `Span` with the appropriate style based on inline state flags.
fn make_inline_span(text: &str, code: bool, bold: bool, italic: bool) -> Span<'static> {
    if code {
        return Span::styled(text.to_string(), Style::default().fg(Color::Cyan));
    }
    let mut style = Style::default();
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if bold || italic {
        Span::styled(text.to_string(), style)
    } else {
        Span::raw(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── hard_wrap tests ────────────────────────────────────────────────

    #[test]
    fn hard_wrap_empty_string() {
        assert_eq!(hard_wrap("", 10), vec![""]);
    }

    #[test]
    fn hard_wrap_shorter_than_width() {
        assert_eq!(hard_wrap("hello", 10), vec!["hello"]);
    }

    #[test]
    fn hard_wrap_exact_width() {
        assert_eq!(hard_wrap("hello", 5), vec!["hello"]);
    }

    #[test]
    fn hard_wrap_longer_with_leading_spaces() {
        let input = "    indented text here";
        let result = hard_wrap(input, 10);
        assert_eq!(result[0], "    indent");
        assert_eq!(result[1], "ed text he");
        assert_eq!(result[2], "re");
    }

    #[test]
    fn hard_wrap_tabs_preserved() {
        let input = "\t\thello world";
        let result = hard_wrap(input, 6);
        assert_eq!(result[0], "\t\thell");
        assert_eq!(result[1], "o worl");
        assert_eq!(result[2], "d");
    }

    #[test]
    fn hard_wrap_multibyte_unicode() {
        // Each emoji is one char but multiple bytes.
        let input = "aaaaa";
        let result = hard_wrap(input, 3);
        assert_eq!(result, vec!["aaa", "aa"]);

        let emoji_input = "\u{1f600}\u{1f601}\u{1f602}\u{1f603}";
        let result = hard_wrap(emoji_input, 2);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].chars().count(), 2);
        assert_eq!(result[1].chars().count(), 2);
    }

    #[test]
    fn hard_wrap_zero_width() {
        assert_eq!(hard_wrap("hello", 0), vec!["hello"]);
    }

    // ── prose_wrap tests ───────────────────────────────────────────────

    #[test]
    fn prose_wrap_word_boundary_breaks() {
        let input = "hello world foo bar";
        let result = prose_wrap(input, 11);
        // Breaks at the space between "hello" and "world".
        assert_eq!(result[0], "hello");
        assert_eq!(result[1], "world foo");
        assert_eq!(result[2], "bar");
    }

    #[test]
    fn prose_wrap_preserves_leading_indent() {
        let input = "    indented text that wraps";
        let result = prose_wrap(input, 16);
        // "    indented" is 12 chars, fits. "    indented tex" is 16.
        // Break at last whitespace before 16 → position 12 (space after "indented").
        assert_eq!(result[0], "    indented");
        assert_eq!(result[1], "text that wraps");
    }

    #[test]
    fn prose_wrap_long_word_fallback() {
        let input = "aaaaabbbbbccccc rest";
        let result = prose_wrap(input, 10);
        // No whitespace in first 10 chars → hard-cut.
        assert_eq!(result[0], "aaaaabbbbb");
        assert_eq!(result[1], "ccccc rest");
    }

    #[test]
    fn prose_wrap_empty_string() {
        assert_eq!(prose_wrap("", 10), vec![""]);
    }

    #[test]
    fn prose_wrap_zero_width() {
        assert_eq!(prose_wrap("hello", 0), vec!["hello"]);
    }

    #[test]
    fn prose_wrap_fits_within_width() {
        assert_eq!(prose_wrap("short", 10), vec!["short"]);
    }

    // ── render_markdown tests ─────────────────────────────────────────

    fn test_gutter() -> Span<'static> {
        Span::raw("G ".to_string())
    }

    /// Extract the raw text of each Line (ignoring styles) for easy assertions.
    fn line_texts(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn md_code_fence_renders_dim() {
        let input = "before\n```rust\nlet x = 1;\n```\nafter";
        let lines = render_markdown(input, 80, test_gutter);
        let texts = line_texts(&lines);
        assert_eq!(texts[0], "G before");
        assert_eq!(texts[1], "G ``` rust");
        assert_eq!(texts[2], "G let x = 1;");
        assert_eq!(texts[3], "G ```");
        assert_eq!(texts[4], "G after");

        // Code line should have DIM modifier.
        let code_span = &lines[2].spans[1];
        assert!(code_span.style.add_modifier.contains(Modifier::DIM));
        assert_eq!(code_span.style.fg, Some(Color::White));
    }

    #[test]
    fn md_code_fence_unclosed_streaming() {
        let input = "```\nline1\nline2";
        let lines = render_markdown(input, 80, test_gutter);
        let texts = line_texts(&lines);
        assert_eq!(texts.len(), 3);
        assert_eq!(texts[0], "G ```");
        // Both remaining lines should be code (DIM).
        assert!(lines[1].spans[1].style.add_modifier.contains(Modifier::DIM));
        assert!(lines[2].spans[1].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn md_headers() {
        let input = "# Title\n## Subtitle\n### Section";
        let lines = render_markdown(input, 80, test_gutter);

        // H1: bold Yellow.
        let h1 = &lines[0].spans[1];
        assert_eq!(h1.content.as_ref(), "Title");
        assert_eq!(h1.style.fg, Some(Color::Yellow));
        assert!(h1.style.add_modifier.contains(Modifier::BOLD));

        // H2: bold Cyan.
        let h2 = &lines[1].spans[1];
        assert_eq!(h2.content.as_ref(), "Subtitle");
        assert_eq!(h2.style.fg, Some(Color::Cyan));
        assert!(h2.style.add_modifier.contains(Modifier::BOLD));

        // H3: bold White.
        let h3 = &lines[2].spans[1];
        assert_eq!(h3.content.as_ref(), "Section");
        assert_eq!(h3.style.fg, Some(Color::White));
        assert!(h3.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn md_bold_and_italic_inline() {
        let spans = parse_inline("hello **bold** and *italic* end");
        // Expected spans: "hello " (plain), "bold" (bold), " and " (plain),
        //                  "italic" (italic), " end" (plain)
        assert_eq!(spans[0].content.as_ref(), "hello ");
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[1].content.as_ref(), "bold");
        assert_eq!(spans[2].content.as_ref(), " and ");
        assert!(spans[3].style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(spans[3].content.as_ref(), "italic");
        assert_eq!(spans[4].content.as_ref(), " end");
    }

    #[test]
    fn md_inline_code() {
        let spans = parse_inline("use `foo` here");
        assert_eq!(spans[0].content.as_ref(), "use ");
        assert_eq!(spans[1].content.as_ref(), "foo");
        assert_eq!(spans[1].style.fg, Some(Color::Cyan));
        assert_eq!(spans[2].content.as_ref(), " here");
    }

    #[test]
    fn md_mixed_prose_and_code_fence() {
        let input = "Hello world\n```\ncode\n```\nGoodbye";
        let lines = render_markdown(input, 80, test_gutter);
        let texts = line_texts(&lines);
        assert_eq!(texts[0], "G Hello world");
        assert_eq!(texts[1], "G ```");
        assert_eq!(texts[2], "G code");
        assert_eq!(texts[3], "G ```");
        assert_eq!(texts[4], "G Goodbye");
    }

    #[test]
    fn md_unordered_list() {
        let input = "- item one\n* item two\n  - nested";
        let lines = render_markdown(input, 80, test_gutter);
        let texts = line_texts(&lines);
        assert!(texts[0].contains("• item one"));
        assert!(texts[1].contains("• item two"));
        assert!(texts[2].contains("• nested"));
    }

    #[test]
    fn md_ordered_list() {
        let input = "1. first\n2. second";
        let lines = render_markdown(input, 80, test_gutter);
        let texts = line_texts(&lines);
        assert!(texts[0].contains("1. first"));
        assert!(texts[1].contains("2. second"));
    }

    #[test]
    fn md_horizontal_rule() {
        let input = "above\n---\nbelow";
        let lines = render_markdown(input, 20, test_gutter);
        let texts = line_texts(&lines);
        assert_eq!(texts[0], "G above");
        // Rule should be repeated '─' chars.
        assert!(lines[1].spans[1].content.contains('─'));
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::DarkGray));
        assert_eq!(texts[2], "G below");
    }

    #[test]
    fn md_horizontal_rule_variants() {
        assert!(is_horizontal_rule("---"));
        assert!(is_horizontal_rule("***"));
        assert!(is_horizontal_rule("___"));
        assert!(is_horizontal_rule("- - -"));
        assert!(is_horizontal_rule("----------"));
        assert!(!is_horizontal_rule("--"));
        assert!(!is_horizontal_rule("abc"));
        assert!(!is_horizontal_rule(""));
    }
}
