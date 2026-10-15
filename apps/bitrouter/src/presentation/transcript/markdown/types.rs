// Adapted from ratatui-markdown 0.3.6, src/markdown/types.rs, by langyo.
// https://github.com/celestia-island/ratatui-markdown
// Modified for BitRouter's backend-independent transcript types and fixed style.
// Distributed under Apache-2.0; see LICENSE and NOTICE in this directory.

#[derive(Debug, Clone, PartialEq)]
pub(super) enum MarkdownBlock {
    Heading1(String),
    Heading2(String),
    Heading3(String),
    Paragraph(Vec<String>),
    CodeBlock(String),
    ListItem(String, u8),
    TaskItem {
        text: String,
        indent: u8,
        checked: bool,
    },
    Blockquote {
        level: u8,
        children: Vec<Self>,
    },
    HorizontalRule,
    BlankLine,
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    // Preserve the text fallback without image loading or placement machinery.
    ImageLabel(String),
}

impl MarkdownBlock {
    pub(super) fn code_block(code: impl Into<String>) -> Self {
        Self::CodeBlock(code.into())
    }
}

#[derive(Debug)]
pub(super) enum TextToken {
    Word(String),
    Space,
    Newline,
}
