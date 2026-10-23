// Adapted from ratatui-markdown 0.3.6, src/markdown/inline.rs, by langyo.
// https://github.com/celestia-island/ratatui-markdown
// Modified for BitRouter's backend-independent transcript types and fixed style.
// Distributed under Apache-2.0; see LICENSE and NOTICE in this directory.

use super::{Format, Run, Tone};

pub(super) fn parse_inline_formatting(text: &str) -> Vec<Run> {
    let expanded = text.replace('\t', "    ");
    let mut spans: Vec<Run> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = expanded.chars().collect();
    let len = chars.len();
    let mut i = 0;

    macro_rules! flush_current {
        () => {
            if !current.is_empty() {
                spans.push(Run::annotated(
                    current.clone(),
                    Format::default().tone(Tone::Text),
                ));
                current.clear();
            }
        };
    }

    while i < len {
        if chars[i] == '*' && i + 2 < len && chars[i + 1] == '*' && chars[i + 2] == '*' {
            flush_current!();
            let start = i + 3;
            let mut found = false;
            let mut end = start;
            while end + 2 < len {
                if chars[end] == '*' && chars[end + 1] == '*' && chars[end + 2] == '*' {
                    let t: String = chars[start..end].iter().collect();
                    spans.push(Run::annotated(
                        t,
                        Format::default().tone(Tone::Text).bold().italic(),
                    ));
                    i = end + 3;
                    found = true;
                    break;
                }
                end += 1;
            }
            if !found {
                current.push('*');
                current.push('*');
                current.push('*');
                i += 3;
            }
            continue;
        }

        if (chars[i] == '*' || chars[i] == '_') && i + 1 < len && chars[i + 1] == chars[i] {
            flush_current!();
            let delimiter = chars[i];
            let start = i + 2;
            let mut end = start;
            let mut found = false;
            while end + 1 < len {
                if chars[end] == delimiter && chars[end + 1] == delimiter {
                    let t: String = chars[start..end].iter().collect();
                    spans.push(Run::annotated(t, Format::default().tone(Tone::Text).bold()));
                    i = end + 2;
                    found = true;
                    break;
                }
                end += 1;
            }
            if !found {
                current.push(chars[i]);
                current.push(chars[i]);
                i += 2;
            }
            continue;
        }

        if chars[i] == '*' || chars[i] == '_' {
            let is_left_flanking = i == 0
                || chars[i - 1] == ' '
                || chars[i - 1] == '\t'
                || chars[i - 1] == '\n'
                || chars[i - 1] == '('
                || chars[i - 1] == '[';
            if !is_left_flanking {
                current.push(chars[i]);
                i += 1;
                continue;
            }
            flush_current!();
            let delimiter = chars[i];
            let start = i + 1;
            let mut end = start;
            let mut found = false;
            while end < len {
                if chars[end] == delimiter {
                    let t: String = chars[start..end].iter().collect();
                    spans.push(Run::annotated(
                        t,
                        Format::default().tone(Tone::Text).italic(),
                    ));
                    i = end + 1;
                    found = true;
                    break;
                }
                end += 1;
            }
            if !found {
                current.push(chars[i]);
                i += 1;
            }
            continue;
        }

        if chars[i] == '`' {
            let start = i + 1;
            let mut end = start;
            let mut found = false;
            while end < len {
                if chars[end] == '`' {
                    found = true;
                    break;
                }
                end += 1;
            }
            if !found {
                current.push('`');
                i += 1;
                continue;
            }
            let need_before = if !current.is_empty() {
                !current.ends_with(' ')
            } else {
                spans.last().is_some_and(|s| !s.content.ends_with(' '))
            };
            if need_before {
                current.push(' ');
            }
            flush_current!();
            let t: String = chars[start..end].iter().collect();
            spans.push(Run::annotated(t, Format::default().tone(Tone::Code)));
            i = end + 1;
            if i < len && chars[i] != ' ' && chars[i] != '\n' {
                current.push(' ');
            }
            continue;
        }

        if chars[i] == '~' && i + 1 < len && chars[i + 1] == '~' {
            let start = i + 2;
            let mut end = start;
            let mut found = false;
            while end + 1 < len {
                if chars[end] == '~' && chars[end + 1] == '~' {
                    let t: String = chars[start..end].iter().collect();
                    flush_current!();
                    spans.push(Run::annotated(
                        t,
                        Format::default().tone(Tone::Text).crossed_out(),
                    ));
                    i = end + 2;
                    found = true;
                    break;
                }
                end += 1;
            }
            if !found {
                current.push('~');
                current.push('~');
                i += 2;
            }
            continue;
        }

        if chars[i] == '[' {
            let mut end_bracket = i + 1;
            let mut found_link = false;
            while end_bracket < len {
                if chars[end_bracket] == ']' {
                    if end_bracket + 1 < len && chars[end_bracket + 1] == '(' {
                        let url_start = end_bracket + 2;
                        let mut url_end = url_start;
                        while url_end < len {
                            if chars[url_end] == ')' {
                                let link_text: String = chars[i + 1..end_bracket].iter().collect();
                                flush_current!();
                                spans.push(Run::annotated(
                                    link_text,
                                    Format::default().tone(Tone::Action).underlined(),
                                ));
                                i = url_end + 1;
                                found_link = true;
                                break;
                            }
                            url_end += 1;
                        }
                    }
                    break;
                }
                end_bracket += 1;
            }
            if found_link {
                continue;
            }
        }

        current.push(chars[i]);
        i += 1;
    }

    if !current.is_empty() {
        spans.push(Run::annotated(current, Format::default().tone(Tone::Text)));
    }

    spans
}
