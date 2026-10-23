// Adapted from ratatui-markdown 0.3.6, src/markdown/parser.rs, by langyo.
// https://github.com/celestia-island/ratatui-markdown
// Modified for BitRouter's backend-independent transcript types and fixed style.
// Distributed under Apache-2.0; see LICENSE and NOTICE in this directory.

use super::{Renderer, types::MarkdownBlock};

const MD_FENCE: &str = "```";
const MD_HRULE_DASH: &str = "---";
const MD_HRULE_STAR: &str = "***";
const MD_HRULE_UNDERSCORE: &str = "___";
const MD_H3: &str = "### ";
const MD_H2: &str = "## ";
const MD_H1: &str = "# ";
const MD_LIST_DASH: &str = "- ";
const MD_LIST_STAR: &str = "* ";
const MD_LIST_PLUS: &str = "+ ";

fn parse_image_syntax(text: &str) -> Option<(String, String)> {
    let trimmed = text.trim();
    if !trimmed.starts_with('!') {
        return None;
    }
    let rest = &trimmed[1..];
    if !rest.starts_with('[') {
        return None;
    }
    let alt_end = rest.find(']')?;
    let alt = rest[1..alt_end].to_string();
    let url_part = &rest[alt_end + 1..];
    if !url_part.starts_with('(') {
        return None;
    }
    let url_end = url_part.find(')')?;
    let path = url_part[1..url_end].to_string();
    if path.is_empty() {
        return None;
    }
    Some((alt, path))
}

fn is_line_only_image(text: &str) -> bool {
    let trimmed = text.trim();
    if !trimmed.starts_with('!') {
        return false;
    }
    if let Some(close_bracket) = trimmed.find(']') {
        let rest = &trimmed[close_bracket + 1..];
        if rest.starts_with('(')
            && let Some(close_paren) = rest.find(')')
        {
            let after = rest[close_paren + 1..].trim();
            return after.is_empty();
        }
    }
    false
}

impl Renderer {
    pub(super) fn parse(markdown: &str) -> Vec<MarkdownBlock> {
        let mut blocks = Vec::new();
        let mut in_code_block = false;
        let mut code_content = String::new();
        let mut paragraph_lines: Vec<String> = Vec::new();
        let mut table_buffer: Vec<String> = Vec::new();

        let mut lines = markdown.lines().peekable();

        while let Some(line) = lines.next() {
            if in_code_block {
                if line.trim().starts_with(MD_FENCE) {
                    in_code_block = false;
                    blocks.push(MarkdownBlock::code_block(code_content.trim_end()));
                    code_content.clear();
                } else {
                    code_content.push_str(line);
                    code_content.push('\n');
                }
                continue;
            }

            if line.trim().starts_with(MD_FENCE) {
                Self::flush_table(&mut table_buffer, &mut blocks, &mut paragraph_lines);
                in_code_block = true;
                continue;
            }

            let trimmed = line.trim();

            if trimmed.is_empty() {
                Self::flush_table(&mut table_buffer, &mut blocks, &mut paragraph_lines);
                if !paragraph_lines.is_empty() {
                    blocks.push(MarkdownBlock::Paragraph(paragraph_lines.clone()));
                    paragraph_lines.clear();
                }
                blocks.push(MarkdownBlock::BlankLine);
                continue;
            }

            if is_line_only_image(trimmed) {
                Self::flush_table(&mut table_buffer, &mut blocks, &mut paragraph_lines);
                if !paragraph_lines.is_empty() {
                    blocks.push(MarkdownBlock::Paragraph(paragraph_lines.clone()));
                    paragraph_lines.clear();
                }
                if let Some((alt, path)) = parse_image_syntax(trimmed) {
                    blocks.push(MarkdownBlock::ImageLabel(if alt.is_empty() {
                        path
                    } else {
                        alt
                    }));
                }
                continue;
            }

            if trimmed.starts_with(MD_HRULE_DASH)
                || trimmed.starts_with(MD_HRULE_STAR)
                || trimmed.starts_with(MD_HRULE_UNDERSCORE)
            {
                Self::flush_table(&mut table_buffer, &mut blocks, &mut paragraph_lines);
                if !paragraph_lines.is_empty() {
                    blocks.push(MarkdownBlock::Paragraph(paragraph_lines.clone()));
                    paragraph_lines.clear();
                }
                blocks.push(MarkdownBlock::HorizontalRule);
                continue;
            }

            if line.starts_with(MD_H3) {
                Self::flush_table(&mut table_buffer, &mut blocks, &mut paragraph_lines);
                if !paragraph_lines.is_empty() {
                    blocks.push(MarkdownBlock::Paragraph(paragraph_lines.clone()));
                    paragraph_lines.clear();
                }
                let text = trimmed.chars().skip(4).collect::<String>();
                blocks.push(MarkdownBlock::Heading3(text));
                continue;
            }

            if line.starts_with(MD_H2) {
                Self::flush_table(&mut table_buffer, &mut blocks, &mut paragraph_lines);
                if !paragraph_lines.is_empty() {
                    blocks.push(MarkdownBlock::Paragraph(paragraph_lines.clone()));
                    paragraph_lines.clear();
                }
                let text = trimmed.chars().skip(3).collect::<String>();
                blocks.push(MarkdownBlock::Heading2(text));
                continue;
            }

            if line.starts_with(MD_H1) {
                Self::flush_table(&mut table_buffer, &mut blocks, &mut paragraph_lines);
                if !paragraph_lines.is_empty() {
                    blocks.push(MarkdownBlock::Paragraph(paragraph_lines.clone()));
                    paragraph_lines.clear();
                }
                let text = trimmed.chars().skip(2).collect::<String>();
                blocks.push(MarkdownBlock::Heading1(text));
                continue;
            }

            if trimmed.starts_with('>') {
                Self::flush_table(&mut table_buffer, &mut blocks, &mut paragraph_lines);
                if !paragraph_lines.is_empty() {
                    blocks.push(MarkdownBlock::Paragraph(paragraph_lines.clone()));
                    paragraph_lines.clear();
                }
                let mut bq_lines: Vec<String> = Vec::new();
                bq_lines.push(trimmed.to_string());

                while let Some(&next) = lines.peek() {
                    let next_trimmed = next.trim();
                    if next_trimmed.is_empty() || !next_trimmed.starts_with('>') {
                        break;
                    }
                    bq_lines.push(next_trimmed.to_string());
                    lines.next();
                }

                let blockquote = Self::parse_blockquote_group(&bq_lines);
                blocks.push(blockquote);
                continue;
            }

            let list_indent = Self::count_list_indent(line);
            if trimmed.starts_with(MD_LIST_DASH)
                || trimmed.starts_with(MD_LIST_STAR)
                || trimmed.starts_with(MD_LIST_PLUS)
            {
                let after_marker: String = trimmed.chars().skip(2).collect::<String>();
                let after_marker_trimmed = after_marker.trim_start();

                if after_marker_trimmed.starts_with("[ ] ")
                    || after_marker_trimmed.starts_with("[x] ")
                    || after_marker_trimmed.starts_with("[X] ")
                {
                    Self::flush_table(&mut table_buffer, &mut blocks, &mut paragraph_lines);
                    if !paragraph_lines.is_empty() {
                        blocks.push(MarkdownBlock::Paragraph(paragraph_lines.clone()));
                        paragraph_lines.clear();
                    }
                    let checked = after_marker_trimmed.starts_with("[x] ")
                        || after_marker_trimmed.starts_with("[X] ");
                    let text = after_marker_trimmed.chars().skip(4).collect::<String>();
                    blocks.push(MarkdownBlock::TaskItem {
                        text,
                        indent: list_indent,
                        checked,
                    });
                    continue;
                }

                Self::flush_table(&mut table_buffer, &mut blocks, &mut paragraph_lines);
                if !paragraph_lines.is_empty() {
                    blocks.push(MarkdownBlock::Paragraph(paragraph_lines.clone()));
                    paragraph_lines.clear();
                }
                let content = trimmed.chars().skip(2).collect::<String>();
                blocks.push(MarkdownBlock::ListItem(content, list_indent));
                continue;
            }

            if let Some(pos) = trimmed.find(". ") {
                let prefix = &trimmed[..pos];
                if pos > 0 && pos < 5 && prefix.parse::<u32>().is_ok() {
                    Self::flush_table(&mut table_buffer, &mut blocks, &mut paragraph_lines);
                    if !paragraph_lines.is_empty() {
                        blocks.push(MarkdownBlock::Paragraph(paragraph_lines.clone()));
                        paragraph_lines.clear();
                    }
                    let content = trimmed[pos + 2..].to_string();
                    blocks.push(MarkdownBlock::ListItem(content, list_indent));
                    continue;
                }
            }

            if Self::is_table_line(trimmed) {
                if !paragraph_lines.is_empty() {
                    blocks.push(MarkdownBlock::Paragraph(paragraph_lines.clone()));
                    paragraph_lines.clear();
                }
                table_buffer.push(trimmed.to_string());
                continue;
            }

            Self::flush_table(&mut table_buffer, &mut blocks, &mut paragraph_lines);
            paragraph_lines.push(trimmed.to_string());
        }

        Self::flush_table(&mut table_buffer, &mut blocks, &mut paragraph_lines);
        if !paragraph_lines.is_empty() {
            blocks.push(MarkdownBlock::Paragraph(paragraph_lines));
        }

        if in_code_block {
            blocks.push(MarkdownBlock::code_block(code_content.trim_end()));
        }

        blocks
    }

    fn parse_blockquote_group(lines: &[String]) -> MarkdownBlock {
        let mut max_level: u8 = 1;
        let mut inner_lines: Vec<(u8, String)> = Vec::new();

        for line in lines {
            let (level, content) = Self::strip_blockquote_prefix(line);
            if level > max_level {
                max_level = level;
            }
            inner_lines.push((level, content));
        }

        if max_level == 1 {
            let children = Self::parse_blockquote_content(&inner_lines);
            return MarkdownBlock::Blockquote { level: 1, children };
        }

        let children = Self::parse_nested_blockquote(&inner_lines, 1);
        MarkdownBlock::Blockquote { level: 1, children }
    }

    fn strip_blockquote_prefix(line: &str) -> (u8, String) {
        let mut level: u8 = 0;
        let rest = line.trim_start();
        let chars: Vec<char> = rest.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            if chars[i] == '>' {
                level = level.saturating_add(1);
                i += 1;
                if i < chars.len() && chars[i] == ' ' {
                    i += 1;
                }
            } else {
                break;
            }
        }

        let content: String = chars[i..].iter().collect();
        (level, content)
    }

    fn parse_nested_blockquote(lines: &[(u8, String)], current_level: u8) -> Vec<MarkdownBlock> {
        let mut children = Vec::new();
        let mut group: Vec<(u8, String)> = Vec::new();

        for (level, content) in lines {
            if *level > current_level {
                group.push((*level, content.clone()));
            } else {
                if !group.is_empty() {
                    let inner = Self::parse_nested_blockquote_inner(&group, current_level + 1);
                    children.push(inner);
                    group.clear();
                }
                children.push(MarkdownBlock::Paragraph(vec![content.clone()]));
            }
        }

        if !group.is_empty() {
            let inner = Self::parse_nested_blockquote_inner(&group, current_level + 1);
            children.push(inner);
        }

        children
    }

    fn parse_nested_blockquote_inner(lines: &[(u8, String)], target_level: u8) -> MarkdownBlock {
        let adjusted: Vec<(u8, String)> = lines
            .iter()
            .map(|(level, content)| (*level, content.clone()))
            .collect();

        let has_deeper = adjusted.iter().any(|(l, _)| *l > target_level);

        if has_deeper {
            let children = Self::parse_nested_blockquote(&adjusted, target_level);
            MarkdownBlock::Blockquote {
                level: target_level,
                children,
            }
        } else {
            let contents: Vec<String> = adjusted
                .iter()
                .map(|(_, c)| c.clone())
                .filter(|c| !c.is_empty())
                .collect();
            MarkdownBlock::Blockquote {
                level: target_level,
                children: vec![MarkdownBlock::Paragraph(contents)],
            }
        }
    }

    fn parse_blockquote_content(lines: &[(u8, String)]) -> Vec<MarkdownBlock> {
        let mut blocks = Vec::new();
        let mut text_lines: Vec<String> = Vec::new();
        let mut in_inner_code = false;
        let mut inner_code_content = String::new();

        for (_, content) in lines {
            let trimmed = content.trim();

            if in_inner_code {
                if trimmed.starts_with(MD_FENCE) {
                    in_inner_code = false;
                    if !text_lines.is_empty() {
                        blocks.push(MarkdownBlock::Paragraph(text_lines.clone()));
                        text_lines.clear();
                    }
                    blocks.push(MarkdownBlock::code_block(inner_code_content.trim_end()));
                    inner_code_content.clear();
                } else {
                    inner_code_content.push_str(trimmed);
                    inner_code_content.push('\n');
                }
                continue;
            }

            if trimmed.starts_with(MD_FENCE) {
                if !text_lines.is_empty() {
                    blocks.push(MarkdownBlock::Paragraph(text_lines.clone()));
                    text_lines.clear();
                }
                in_inner_code = true;
                continue;
            }

            if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
                if !text_lines.is_empty() {
                    blocks.push(MarkdownBlock::Paragraph(text_lines.clone()));
                    text_lines.clear();
                }
                let item_text: String = trimmed.chars().skip(2).collect();
                blocks.push(MarkdownBlock::ListItem(item_text, 0));
                continue;
            }

            if !trimmed.is_empty() {
                text_lines.push(trimmed.to_string());
            } else if !text_lines.is_empty() {
                blocks.push(MarkdownBlock::Paragraph(text_lines.clone()));
                text_lines.clear();
            }
        }

        if in_inner_code {
            blocks.push(MarkdownBlock::code_block(inner_code_content.trim_end()));
        }

        if !text_lines.is_empty() {
            blocks.push(MarkdownBlock::Paragraph(text_lines));
        }

        if blocks.is_empty() {
            let all_text: Vec<String> = lines
                .iter()
                .map(|(_, c)| c.clone())
                .filter(|c| !c.is_empty())
                .collect();
            if !all_text.is_empty() {
                blocks.push(MarkdownBlock::Paragraph(all_text));
            }
        }

        blocks
    }

    fn is_table_line(line: &str) -> bool {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.contains('|') {
            return false;
        }
        if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 1 {
            return true;
        }
        let pipe_count = trimmed.chars().filter(|&c| c == '|').count();
        if pipe_count >= 2 {
            let non_sep = trimmed
                .chars()
                .filter(|c| *c != '|' && *c != '-' && *c != ':' && *c != ' ')
                .count();
            if non_sep > 0 {
                return true;
            }
            let sep_chars: Vec<char> = trimmed.chars().filter(|c| *c != '|' && *c != ' ').collect();
            if !sep_chars.is_empty() && sep_chars.iter().all(|c| *c == '-' || *c == ':') {
                return true;
            }
        }
        false
    }

    fn flush_table(
        table_buffer: &mut Vec<String>,
        blocks: &mut Vec<MarkdownBlock>,
        paragraph_lines: &mut Vec<String>,
    ) {
        if table_buffer.is_empty() {
            return;
        }
        if table_buffer.len() < 2 {
            for line in table_buffer.drain(..) {
                paragraph_lines.push(line);
            }
            return;
        }
        let separator_idx = table_buffer.iter().position(|l| {
            l.chars()
                .all(|c| c == '|' || c == '-' || c == ':' || c == ' ')
        });
        let Some(sep_pos) = separator_idx.filter(|&index| index > 0) else {
            for line in table_buffer.drain(..) {
                paragraph_lines.push(line);
            }
            return;
        };
        let headers = Self::split_table_row(&table_buffer[sep_pos - 1]);
        let sep = sep_pos + 1;
        let rows: Vec<Vec<String>> = if sep < table_buffer.len() {
            table_buffer[sep..]
                .iter()
                .filter(|l| {
                    !l.chars()
                        .all(|c| c == '|' || c == '-' || c == ':' || c == ' ')
                })
                .map(|l| Self::split_table_row(l))
                .collect()
        } else {
            vec![]
        };
        blocks.push(MarkdownBlock::Table { headers, rows });
        table_buffer.clear();
    }

    fn split_table_row(line: &str) -> Vec<String> {
        let trimmed = line.trim();
        let inner = if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 1 {
            &trimmed[1..trimmed.len() - 1]
        } else if let Some(rest) = trimmed.strip_prefix('|') {
            rest
        } else if trimmed.ends_with('|') && trimmed.len() > 1 {
            &trimmed[..trimmed.len() - 1]
        } else {
            trimmed
        };
        inner
            .split('|')
            .map(|cell| cell.trim().to_string())
            .collect()
    }

    fn count_list_indent(line: &str) -> u8 {
        let spaces = line.chars().take_while(|&c| c == ' ').count();
        (spaces / 2).min(255) as u8
    }
}
