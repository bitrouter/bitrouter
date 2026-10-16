//! Transcript text and semantic emphasis, independent of a terminal backend.

use std::fmt;

use unicode_width::UnicodeWidthStr as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Text,
    Action,
    Thought,
    Muted,
    Code,
    Success,
    Pending,
    Error,
    Unknown,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Format {
    pub tone: Option<Tone>,
    pub bold: bool,
    pub italic: bool,
    pub underlined: bool,
    pub crossed_out: bool,
}

impl Format {
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = Some(tone);
        self
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub fn underlined(mut self) -> Self {
        self.underlined = true;
        self
    }

    pub fn crossed_out(mut self) -> Self {
        self.crossed_out = true;
        self
    }

    /// A containing voice supplies tone without discarding inline emphasis.
    pub fn patch(self, overlay: Self) -> Self {
        Self {
            tone: overlay.tone.or(self.tone),
            bold: self.bold || overlay.bold,
            italic: self.italic || overlay.italic,
            underlined: self.underlined || overlay.underlined,
            crossed_out: self.crossed_out || overlay.crossed_out,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Run {
    pub content: String,
    pub format: Format,
}

impl Run {
    pub fn raw(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            format: Format::default(),
        }
    }

    pub fn annotated(content: impl Into<String>, format: Format) -> Self {
        Self {
            content: content.into(),
            format,
        }
    }

    pub fn width(&self) -> usize {
        self.content.width()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub runs: Vec<Run>,
}

impl Row {
    pub fn annotated(content: impl Into<String>, format: Format) -> Self {
        // Preserve the previous row constructor's line-separator handling.
        // An explicit Run instead retains its content byte for byte.
        // https://github.com/ratatui/ratatui/blob/v0.29.0/src/text/line.rs
        Self {
            runs: content
                .into()
                .lines()
                .map(|line| Run::annotated(line, format))
                .collect(),
        }
    }

    pub fn width(&self) -> usize {
        self.runs.iter().map(Run::width).sum()
    }
}

impl From<String> for Row {
    fn from(content: String) -> Self {
        Self::annotated(content, Format::default())
    }
}

impl From<&str> for Row {
    fn from(content: &str) -> Self {
        Self::annotated(content, Format::default())
    }
}

impl From<Run> for Row {
    fn from(run: Run) -> Self {
        Self { runs: vec![run] }
    }
}

impl From<Vec<Run>> for Row {
    fn from(runs: Vec<Run>) -> Self {
        Self { runs }
    }
}

impl fmt::Display for Row {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for run in &self.runs {
            formatter.write_str(&run.content)?;
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extent {
    pub width: u16,
    pub height: u16,
}
