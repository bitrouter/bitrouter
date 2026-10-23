// Adapted from ratatui-markdown 0.3.6, src/markdown/text.rs, by langyo.
// https://github.com/celestia-island/ratatui-markdown
// Modified for BitRouter's backend-independent transcript types and fixed style.
// Distributed under Apache-2.0; see LICENSE and NOTICE in this directory.

use unicode_width::UnicodeWidthChar;

use super::{Renderer, Row, Run, inline::parse_inline_formatting, types::TextToken};

impl Renderer {
    pub(super) fn wrap_text_with_inline_formatting(&self, text: &str) -> Vec<Row> {
        let spans = parse_inline_formatting(text);
        self.wrap_styled_spans_to_lines(spans)
    }

    fn wrap_styled_spans_to_lines(&self, spans: Vec<Run>) -> Vec<Row> {
        if self.max_width == 0 {
            return if spans.is_empty() {
                vec![Row::from("")]
            } else {
                vec![Row::from(spans)]
            };
        }

        let mut lines: Vec<Row> = Vec::new();
        let mut current_line: Vec<Run> = Vec::new();
        let mut current_width: usize = 0;
        let mut pending_space = false;

        for span in spans {
            let style = span.format;
            let text = span.content.to_string();
            let tokens = Self::tokenize(&text);

            for token in tokens {
                match token {
                    TextToken::Newline => {
                        lines.push(Row::from(std::mem::take(&mut current_line)));
                        current_width = 0;
                        pending_space = false;
                    }
                    TextToken::Space => {
                        pending_space = true;
                    }
                    TextToken::Word(word) => {
                        let word_w = Self::string_width(&word);
                        let space_w: usize = if pending_space && current_width > 0 {
                            1
                        } else {
                            0
                        };

                        let needs_wrap = if current_width == 0 {
                            false
                        } else if space_w > 0 && current_width + space_w >= self.max_width {
                            true
                        } else {
                            current_width + space_w + word_w > self.max_width
                        };

                        if needs_wrap {
                            lines.push(Row::from(std::mem::take(&mut current_line)));
                            current_width = 0;
                            pending_space = false;
                        }

                        if pending_space && current_width > 0 {
                            current_line.push(Run::annotated(" ", style));
                            current_width += 1;
                        }
                        pending_space = false;

                        if word_w > self.max_width {
                            let mut buf = String::new();
                            let mut buf_w = 0usize;
                            for ch in word.chars() {
                                let cw = Self::display_width(ch);
                                if current_width + buf_w + cw > self.max_width
                                    && (current_width > 0 || buf_w > 0)
                                {
                                    if !buf.is_empty() {
                                        current_line.push(Run::annotated(buf.clone(), style));
                                        buf.clear();
                                    }
                                    lines.push(Row::from(std::mem::take(&mut current_line)));
                                    current_width = 0;
                                    buf_w = 0;
                                }
                                buf.push(ch);
                                buf_w += cw;
                            }
                            if !buf.is_empty() {
                                current_line.push(Run::annotated(buf, style));
                                current_width += buf_w;
                            }
                        } else {
                            current_line.push(Run::annotated(word, style));
                            current_width += word_w;
                        }
                    }
                }
            }
        }

        if !current_line.is_empty() {
            lines.push(Row::from(current_line));
        }

        if lines.is_empty() {
            lines.push(Row::from(""));
        }

        lines
    }

    pub(super) fn tokenize(text: &str) -> Vec<TextToken> {
        let text = text.replace('\t', "    ");
        let mut tokens = Vec::new();
        let mut current_word = String::new();

        for c in text.chars() {
            if c == '\n' {
                if !current_word.is_empty() {
                    tokens.push(TextToken::Word(current_word));
                    current_word = String::new();
                }
                tokens.push(TextToken::Newline);
                continue;
            }

            if c == ' ' {
                if !current_word.is_empty() {
                    tokens.push(TextToken::Word(current_word));
                    current_word = String::new();
                }
                tokens.push(TextToken::Space);
                continue;
            }

            let is_cjk = Self::is_cjk(c);

            if is_cjk {
                if !current_word.is_empty() {
                    tokens.push(TextToken::Word(current_word));
                    current_word = String::new();
                }
                tokens.push(TextToken::Word(c.to_string()));
            } else {
                current_word.push(c);
            }
        }

        if !current_word.is_empty() {
            tokens.push(TextToken::Word(current_word));
        }

        tokens
    }

    pub(super) fn display_width(c: char) -> usize {
        // Characters without a display width contribute zero cells.
        UnicodeWidthChar::width(c).into_iter().sum()
    }

    pub(super) fn string_width(s: &str) -> usize {
        s.chars().map(Self::display_width).sum()
    }

    fn is_cjk(c: char) -> bool {
        let cp = c as u32;
        (0x4E00..=0x9FFF).contains(&cp)
            || (0x3400..=0x4DBF).contains(&cp)
            || (0x20000..=0x2CEAF).contains(&cp)
            || (0x3040..=0x309F).contains(&cp)
            || (0x30A0..=0x30FF).contains(&cp)
            || (0xAC00..=0xD7AF).contains(&cp)
            || (0x1100..=0x11FF).contains(&cp)
    }
}
