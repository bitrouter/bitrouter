//! Regression tests retained from BitRouter’s formatted Markdown adapter.

use super::*;

fn text(lines: &[Row]) -> String {
    lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn markdown_formats_prose_tables_links_and_literal_code() {
    let source = "## Workspace\n\n**Router** and *cloud*. Use `bitrouter`.\n\n- First\n- Second\n\n| Repo | Role |\n|---|---|\n| [bitrouter](/tmp/README.md) | Router |\n\n```sh\necho '**literal**'\n```";
    let lines = render(source, 80);
    let output = text(&lines);
    assert!(output.starts_with("Workspace\n"), "{output}");
    assert!(
        !output.contains("**Router**") && !output.contains("*cloud*"),
        "{output}"
    );
    assert!(
        !output.contains("[bitrouter]") && !output.contains("/tmp/README.md"),
        "{output}"
    );
    assert!(
        output.contains("┬") && output.contains("Repo") && output.contains("Role"),
        "{output}"
    );
    assert!(output.contains("echo '**literal**'"), "{output}");
    assert!(!output.contains("```"), "{output}");
    assert!(
        lines
            .iter()
            .flat_map(|line| &line.runs)
            .any(|span| span.content == "Router" && span.format.bold)
    );
}

#[test]
fn narrow_tables_retain_each_labeled_value_and_hide_link_syntax() {
    let source = "| Repository | Role |\n|---|---|\n| [bitrouter](/Users/example/a/very/long/workspace/path/README.md) | Routes requests safely |\n| cloud | Hosted platform |";
    let lines = render(source, 24);
    let output = text(&lines);
    assert!(lines.iter().all(|line| line.width() <= 24), "{output}");
    assert!(output.contains("Repository: bitrouter"), "{output}");
    assert!(output.contains("Role: Routes requests"), "{output}");
    assert!(
        output.contains("safely") && output.contains("Hosted platform"),
        "{output}"
    );
    assert!(
        !output.contains("/Users/") && !output.contains("[bitrouter]"),
        "{output}"
    );
}

#[test]
fn streaming_reparses_completed_formatting_and_unclosed_fences() {
    let partial = render("**Inspecting", 40);
    assert!(text(&partial).contains("Inspecting"));
    assert_eq!(
        text(&render("**Inspecting files**", 40)),
        "Inspecting files"
    );
    let code = text(&render("```sh\nprintf '**raw**'", 40));
    assert!(code.contains("printf '**raw**'"), "{code}");
    assert!(code.contains('┌') && code.contains('└'), "{code}");
}

#[test]
fn command_frames_preserve_wrapped_graphemes_and_indentation() {
    let code = "  echo '你好👩‍💻**raw**'";
    let lines = code_block(code, 16);
    assert!(
        lines.iter().all(|line| line.width() == 16),
        "{}",
        text(&lines)
    );
    let restored: String = lines
        .iter()
        .skip(1)
        .take(lines.len() - 2)
        .map(|line| {
            line.to_string()
                .trim_start_matches("│ ")
                .trim_end_matches(" │")
                .trim_end()
                .to_string()
        })
        .collect();
    assert_eq!(restored, code);
    assert!(
        lines
            .iter()
            .flat_map(|line| &line.runs)
            .all(|span| !span.format.bold)
    );
}

// These semantic assertions cover the used upstream render_tests.rs behavior:
// https://github.com/celestia-island/ratatui-markdown (0.3.6)
// Adapted to inspect transcript runs instead of a terminal buffer.
#[test]
fn inline_emphasis_and_tones_survive_wrapping() {
    let rows = render(
        "plain **bold** __strong__ *italic* _slant_ ***both*** ~~gone~~ [link](https://example.com) `code`",
        12,
    );
    for (content, format) in [
        ("plain", Format::default().tone(Tone::Text)),
        ("bold", Format::default().tone(Tone::Text).bold()),
        ("strong", Format::default().tone(Tone::Text).bold()),
        ("italic", Format::default().tone(Tone::Text).italic()),
        ("slant", Format::default().tone(Tone::Text).italic()),
        ("both", Format::default().tone(Tone::Text).bold().italic()),
        ("gone", Format::default().tone(Tone::Text).crossed_out()),
        ("link", Format::default().tone(Tone::Action).underlined()),
        ("code", Format::default().tone(Tone::Code)),
    ] {
        assert!(
            rows.iter()
                .flat_map(|row| &row.runs)
                .any(|run| { run.content == content && run.format == format }),
            "missing {content:?} with {format:?}: {rows:?}"
        );
    }
    assert!(rows.iter().all(|row| row.width() <= 12));
}

#[test]
fn headings_patch_inline_format_over_heading_format() {
    // Upstream inline text explicitly supplies the text tone, so it wins over
    // H1's primary and H3's secondary tone. Heading emphasis remains additive.
    for (source, underlined) in [
        ("# Head *slant* [link](url) `code`", true),
        ("## Head *slant* [link](url) `code`", false),
        ("### Head *slant* [link](url) `code`", false),
    ] {
        let rows = render(source, 4);
        assert_eq!(rows.len(), 1, "headings retain upstream's unwrapped layout");
        let runs = &rows[0].runs;
        assert!(runs.iter().all(|run| run.format.bold));
        assert!(runs.iter().any(|run| run.content == "Head "
            && run.format.tone == Some(Tone::Text)
            && run.format.underlined == underlined));
        assert!(
            runs.iter()
                .any(|run| run.content == "slant" && run.format.italic)
        );
        assert!(runs.iter().any(|run| run.content == "link"
            && run.format.tone == Some(Tone::Action)
            && run.format.underlined));
        assert!(
            runs.iter()
                .any(|run| run.content == "code" && run.format.tone == Some(Tone::Code))
        );
    }
}

#[test]
fn streamed_empty_headings_keep_their_explicit_styled_run() {
    for (source, format) in [
        (
            "# ",
            Format::default().tone(Tone::Action).bold().underlined(),
        ),
        ("## ", Format::default().tone(Tone::Text).bold()),
        ("### ", Format::default().tone(Tone::Thought).bold()),
    ] {
        assert_eq!(
            render(source, 1),
            vec![Row::from(Run::annotated("", format))]
        );
    }
}

#[test]
fn quotes_override_tone_without_losing_inline_emphasis() {
    let rows = render(
        "> **bold** *slant* ~~gone~~ [link](url) `code`\n>\n> - item",
        80,
    );
    assert!(rows.iter().all(|row| row.to_string().starts_with("│ ")));
    assert!(
        rows.iter()
            .flat_map(|row| &row.runs)
            .all(|run| { run.format.tone == Some(Tone::Muted) })
    );
    for row in &rows {
        assert!(!row.runs[0].format.italic);
        assert!(row.runs.iter().skip(1).all(|run| run.format.italic));
    }
    for (content, format) in [
        ("bold", Format::default().tone(Tone::Muted).italic().bold()),
        (
            "gone",
            Format::default().tone(Tone::Muted).italic().crossed_out(),
        ),
        (
            "link",
            Format::default().tone(Tone::Muted).italic().underlined(),
        ),
    ] {
        assert!(
            rows.iter()
                .flat_map(|row| &row.runs)
                .any(|run| { run.content == content && run.format == format })
        );
    }
}

#[test]
fn nested_quotes_and_quoted_code_keep_existing_prefixes() {
    assert_eq!(
        text(&render("> outer\n>> inner\n> tail", 40)),
        "│ outer\n│ │ │ inner\n│ tail"
    );
    let rows = render("> ```rust\n>   **literal**\n> ```", 20);
    assert_eq!(rows.len(), 3);
    assert!(rows[0].to_string().starts_with("│ ┌"));
    assert!(rows[1].to_string().contains("**literal**"));
    assert!(rows[2].to_string().starts_with("│ └"));
    assert!(
        rows.iter()
            .flat_map(|row| &row.runs)
            .all(|run| !run.format.bold)
    );
}

#[test]
fn lists_tasks_rules_and_blank_lines_keep_existing_layout() {
    let source = "- one\n  * two\n+ three\n12. four\n- [ ] todo\n- [x] done\n- [X] also\n\n---";
    assert_eq!(
        text(&render(source, 20)),
        "• one\n• two\n• three\n• four\n☐ todo\n☑ done\n☑ also\n\n────────────────────"
    );
    assert_eq!(render("", 0), vec![Row::default()]);
    assert_eq!(text(&render("ab", 0)), "a\nb");
    assert_eq!(render("---", 100)[0].width(), 80);
}

#[test]
fn incomplete_delimiters_and_intraword_underscores_remain_text() {
    for source in [
        "**unfinished",
        "*unfinished",
        "~~unfinished",
        "`unfinished",
        "[link](unfinished",
        "snake_case_word",
    ] {
        assert_eq!(text(&render(source, 80)), source);
    }
    assert_eq!(text(&render("before`code`after", 80)), "before code after");
}

#[test]
fn tables_keep_inline_styles_and_handle_missing_cells() {
    let rows = render(
        "| *Heading* | Other |\n|---|---|\n| **bold** | ~~gone~~ |\n| [link](url) |",
        80,
    );
    assert!(rows[0].to_string().starts_with('┌'));
    assert!(
        rows.last()
            .is_some_and(|row| row.to_string().ends_with('┘'))
    );
    assert!(rows.iter().all(|row| row.width() == rows[0].width()));
    for (content, format) in [
        (
            "Heading",
            Format::default().tone(Tone::Text).bold().italic(),
        ),
        ("bold", Format::default().tone(Tone::Text).bold()),
        ("gone", Format::default().tone(Tone::Text).crossed_out()),
        ("link", Format::default().tone(Tone::Action).underlined()),
    ] {
        assert!(
            rows.iter()
                .flat_map(|row| &row.runs)
                .any(|run| run.content == content && run.format == format)
        );
    }
}

#[test]
fn header_only_tables_do_not_use_record_fallback() {
    let rows = render("| Long header | Other |\n|---|---|", 4);
    assert!(rows[0].to_string().starts_with('┌'));
    assert!(rows.iter().any(|row| row.width() > 4));
    assert!(!text(&rows).contains("header:"));
}

#[test]
fn image_placeholders_are_text_only_and_preserve_fallback_style() {
    let rows = render("![alt](image.png)\n![](path.png)", 40);
    assert_eq!(text(&rows), "[image: alt]\n[image: path.png]");
    assert!(
        rows.iter()
            .flat_map(|row| &row.runs)
            .all(|run| { run.format == Format::default().tone(Tone::Code).italic() })
    );
}

#[test]
fn code_frames_keep_whitespace_and_override_every_language() {
    assert_eq!(
        text(&code_block("a\n", 6)),
        "┌────┐\n│ a  │\n│    │\n└────┘"
    );
    assert_eq!(text(&code_block("\t x", 5)), "     \nx");
    assert_eq!(text(&code_block("a\nb", 0)), "a\nb");
    assert_eq!(text(&code_block("", 5)), "");
    for lang in ["", "rust", "mermaid", "unknown"] {
        let rows = render(&format!("```{lang}\n**literal**\n```"), 20);
        assert_eq!(rows, code_block("**literal**", 20));
        assert!(
            rows.iter()
                .flat_map(|row| &row.runs)
                .all(|run| { run.format == Format::default().tone(Tone::Muted) })
        );
    }
}

#[test]
fn deeply_nested_quote_prefixes_do_not_overflow_the_depth_counter() {
    let source = format!("{} text", ">".repeat(256));
    let rows = render(&source, 10);
    assert!(text(&rows).ends_with("text"));
}
