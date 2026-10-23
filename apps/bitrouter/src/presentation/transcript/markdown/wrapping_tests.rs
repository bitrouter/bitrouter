// Adapted from ratatui-markdown 0.3.6, src/markdown/tests.rs, by langyo.
// https://github.com/celestia-island/ratatui-markdown
// Modified to test the styled wrapping path used by BitRouter.
// Distributed under Apache-2.0; see LICENSE and NOTICE in this directory.

use super::{Format, Renderer, Run, Tone};

fn renderer(max_width: usize) -> Renderer {
    Renderer { max_width }
}

fn wrap(renderer: &Renderer, text: &str) -> Vec<String> {
    renderer
        .wrap_text_with_inline_formatting(text)
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn wraps_by_word_and_keeps_ascii_token_intact() {
    let renderer = renderer(8);
    let lines = wrap(&renderer, "搜索型 AI 公司");
    assert_eq!(lines, vec!["搜索型", "AI 公司"]);
}

#[test]
fn long_ascii_token_falls_back_without_losing_chars() {
    let renderer = renderer(4);
    let lines = wrap(&renderer, "company");
    assert_eq!(lines.join(""), "company");
}

#[test]
fn ai_word_never_split() {
    let renderer = renderer(7);
    let lines = wrap(&renderer, "搜索型 AI公司");
    assert!(!lines[0].ends_with('A'));
    assert!(lines.iter().any(|l| l.contains("AI")));
}

#[test]
fn narrow_width_keeps_short_words_intact() {
    let renderer = renderer(3);
    let lines = wrap(&renderer, "AI ML DL");
    assert_eq!(lines, vec!["AI", "ML", "DL"]);
}

#[test]
fn single_word_on_narrow_line() {
    let renderer = renderer(1);
    let lines = wrap(&renderer, "AI");
    assert_eq!(lines, vec!["A", "I"]);
}

#[test]
fn ai_word_stays_together_on_reasonable_width() {
    let renderer = renderer(2);
    let lines = wrap(&renderer, "AI");
    assert_eq!(lines, vec!["AI"]);
}

#[test]
fn perplexity_desc_width8() {
    let renderer = renderer(8);
    let lines = wrap(&renderer, "搜索型 AI 公司");
    let joined: String = lines.join("");
    assert_eq!(joined.replace(' ', ""), "搜索型AI公司");
    assert!(lines.iter().any(|l| l.contains("AI")));
    for l in &lines {
        assert!(!l.trim().ends_with('A'), "line ends with lone A: {:?}", l);
        assert!(
            !l.trim().starts_with('I'),
            "line starts with lone I: {:?}",
            l
        );
    }
}

#[test]
fn perplexity_desc_width7_ai_intact() {
    let renderer = renderer(7);
    let lines = wrap(&renderer, "搜索型 AI 公司");
    let joined: String = lines.join("");
    assert_eq!(joined.replace(' ', ""), "搜索型AI公司");
    for l in &lines {
        assert!(!l.trim().ends_with('A'), "lone A at end: {:?}", l);
        assert!(!l.trim().starts_with('I'), "lone I at start: {:?}", l);
    }
}

#[test]
fn perplexity_desc_width10_fits_one_line() {
    let renderer = renderer(10);
    let lines = wrap(&renderer, "AI 公司");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0], "AI 公司");
}

#[test]
fn cjk_chars_break_individually() {
    let renderer = renderer(2);
    let lines = wrap(&renderer, "你好世");
    assert_eq!(lines, vec!["你", "好", "世"]);
}

#[test]
fn cjk_then_ascii_no_overflow() {
    let renderer = renderer(4);
    let lines = wrap(&renderer, "大型LLM");
    for l in &lines {
        let w: usize = l.chars().map(Renderer::display_width).sum();
        assert!(w <= 4, "line {:?} display width {} > 4", l, w);
    }
    assert_eq!(lines.join(""), "大型LLM");
}

#[test]
fn fullwidth_punctuation_breaks_individually() {
    let renderer = renderer(4);
    let lines = wrap(&renderer, "！！！");
    for l in &lines {
        let w: usize = l.chars().map(Renderer::display_width).sum();
        assert!(w <= 4, "line {:?} display width {} > 4", l, w);
    }
    assert_eq!(lines.join(""), "！！！");
}

#[test]
fn no_char_loss_various_widths() {
    let cases: &[(&str, usize)] = &[
        ("搜索型 AI 公司是知名品牌", 6),
        ("Perplexity AI is a search company", 10),
        ("GPT-4o is fast", 5),
        ("中文English混合 token wrap", 8),
    ];
    for (text, width) in cases {
        let renderer = renderer(*width);
        let lines = wrap(&renderer, text);
        let original_chars: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let wrapped_chars: String = lines
            .join("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert_eq!(
            original_chars, wrapped_chars,
            "chars lost wrapping {:?} at width {}",
            text, width
        );
    }
}

#[test]
fn no_line_exceeds_max_width() {
    let cases: &[(&str, usize)] = &[
        ("搜索型 AI 公司", 8),
        ("Hello world from TUI markdown", 10),
        ("你好世界 Hello World", 6),
    ];
    for (text, width) in cases {
        let renderer = renderer(*width);
        let lines = wrap(&renderer, text);
        for l in &lines {
            let w: usize = l.chars().map(Renderer::display_width).sum();
            assert!(
                w <= *width,
                "line {:?} has display width {} > {} (text={:?})",
                l,
                w,
                width,
                text
            );
        }
    }
}

#[test]
fn empty_string_returns_single_empty_line() {
    let renderer = renderer(10);
    let lines = wrap(&renderer, "");
    assert_eq!(lines, vec![""]);
}

#[test]
fn max_width_zero_returns_input_as_single_line() {
    let renderer = renderer(0);
    let lines = wrap(&renderer, "AI 公司");
    assert_eq!(lines, vec!["AI 公司"]);
}

#[test]
fn explicit_newline_in_input_creates_new_line() {
    let renderer = renderer(20);
    let lines = wrap(&renderer, "first\nsecond");
    assert_eq!(lines, vec!["first", "second"]);
}

#[test]
fn long_url_no_char_loss() {
    let url = "https://example.com/very/long/path";
    let renderer = renderer(8);
    let lines = wrap(&renderer, url);
    assert_eq!(lines.join(""), url);
}

#[test]
fn list_item_prefix_with_mixed_text() {
    let renderer = renderer(8);
    let lines = wrap(&renderer, "• AI 技术");
    let joined: String = lines.join("");
    assert!(joined.contains("AI"), "AI should not be split");
    assert!(joined.contains("技"), "CJK chars should not be lost");
}

#[test]
fn table_hline_and_row_same_display_width() {
    let col_widths: Vec<usize> = vec![10, 8, 12];
    let hline = Renderer::build_table_hline(&col_widths, "┌", "┬", "┐");
    let hline_w: usize = hline.chars().map(Renderer::display_width).sum();
    assert_eq!(hline_w, 10 + 8 + 12 + 4);

    let cells = ["abc".to_string(), "de".to_string(), "fgh".to_string()];
    let cell_spans: Vec<Vec<Run>> = cells
        .iter()
        .map(|s| {
            vec![Run::annotated(
                s.clone(),
                Format::default().tone(Tone::Text),
            )]
        })
        .collect();
    let row = Renderer::build_table_row_from_spans(&col_widths, &cell_spans, false);
    let row_text: String = row.runs.iter().map(|s| s.content.as_str()).collect();
    let row_w: usize = row_text.chars().map(Renderer::display_width).sum();
    assert_eq!(
        row_w, hline_w,
        "row display width {} != hline display width {} (row={:?})",
        row_w, hline_w, row_text
    );
}

#[test]
fn table_cjk_cells_aligned() {
    let col_widths: Vec<usize> = vec![12, 12];
    let cells = ["你好世界".to_string(), "テスト".to_string()];
    let cell_spans: Vec<Vec<Run>> = cells
        .iter()
        .map(|s| {
            vec![Run::annotated(
                s.clone(),
                Format::default().tone(Tone::Text),
            )]
        })
        .collect();
    let row = Renderer::build_table_row_from_spans(&col_widths, &cell_spans, false);
    let row_text: String = row.runs.iter().map(|s| s.content.as_str()).collect();
    let row_w: usize = row_text.chars().map(Renderer::display_width).sum();
    let hline = Renderer::build_table_hline(&col_widths, "┌", "┬", "┐");
    let hline_w: usize = hline.chars().map(Renderer::display_width).sum();
    assert_eq!(
        row_w, hline_w,
        "CJK row misaligned: row={:?} hline={:?}",
        row_text, hline
    );
}
