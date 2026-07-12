//! Human-readable render toolkit for [`CliReport`](super::CliReport)s.
//!
//! `render()` implementations compose their human view out of these
//! primitives — they never `println!` directly. Centralising the palette and
//! the layout primitives keeps every command's `--human` output consistent and
//! keeps colour gating (TTY + `NO_COLOR`) in one place.

use std::fmt::Display;
use std::io::{IsTerminal, Write};

/// ANSI palette, chosen once per render. When colour is suppressed (the target
/// stream is not a TTY, or `NO_COLOR` is set) every field is `""` so format
/// strings stay valid but emit nothing. Folded from the former `style.rs`.
#[derive(Clone, Copy)]
pub struct Theme {
    red: &'static str,
    green: &'static str,
    cyan: &'static str,
    bold: &'static str,
    dim: &'static str,
    reset: &'static str,
}

impl Theme {
    /// Full ANSI palette.
    pub fn ansi() -> Self {
        Self {
            red: "\x1b[31m",
            green: "\x1b[32m",
            cyan: "\x1b[36m",
            bold: "\x1b[1m",
            dim: "\x1b[2m",
            reset: "\x1b[0m",
        }
    }

    /// Empty palette — every field is `""`. Used for redirected streams,
    /// `NO_COLOR`, and tests.
    pub fn none() -> Self {
        Self {
            red: "",
            green: "",
            cyan: "",
            bold: "",
            dim: "",
            reset: "",
        }
    }

    /// Palette for stdout (the human result surface). Plain when piped.
    pub fn for_stdout() -> Self {
        if no_color() || !std::io::stdout().is_terminal() {
            Self::none()
        } else {
            Self::ansi()
        }
    }

    /// Palette for stderr (diagnostics / error echoes).
    pub fn for_stderr() -> Self {
        if no_color() || !std::io::stderr().is_terminal() {
            Self::none()
        } else {
            Self::ansi()
        }
    }
}

/// True when `NO_COLOR` is set to a non-empty value (<https://no-color.org/>).
fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

/// Health indicator for [`Human::status_block`] — drives the `●` / `○` glyph
/// and its colour.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Green `●` — the subject is up / healthy.
    Up,
    /// Dim `○` — the subject is down / stopped.
    Down,
    /// Dim `●` — state could not be determined.
    Unknown,
}

/// A fixed-width text table: a header row plus data rows, columns auto-sized to
/// their widest cell, last column left unpadded.
#[derive(Default)]
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// Start a table with the given header cells.
    pub fn new<I, S>(headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            headers: headers.into_iter().map(Into::into).collect(),
            rows: Vec::new(),
        }
    }

    /// Append a data row (builder form).
    pub fn row<I, S>(mut self, cells: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.push(cells);
        self
    }

    /// Append a data row (mutating form).
    pub fn push<I, S>(&mut self, cells: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.rows.push(cells.into_iter().map(Into::into).collect());
    }

    fn widths(&self) -> Vec<usize> {
        let mut w: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < w.len() {
                    w[i] = w[i].max(cell.chars().count());
                }
            }
        }
        w
    }
}

/// A render target wrapping a writer plus a [`Theme`]. Passed to every
/// [`CliReport::render`](super::CliReport::render).
pub struct Human<'a> {
    w: &'a mut dyn Write,
    theme: Theme,
}

impl<'a> Human<'a> {
    /// Wrap a writer with a palette.
    pub fn new(w: &'a mut dyn Write, theme: Theme) -> Self {
        Self { w, theme }
    }

    /// Write one verbatim line.
    pub fn line(&mut self, s: &str) -> std::io::Result<()> {
        writeln!(self.w, "{s}")
    }

    /// Write a blank line.
    pub fn blank(&mut self) -> std::io::Result<()> {
        writeln!(self.w)
    }

    /// A `  label    value` detail row (label dim, left-padded to 8).
    pub fn field(&mut self, label: &str, value: impl Display) -> std::io::Result<()> {
        let (dim, reset) = (self.theme.dim, self.theme.reset);
        writeln!(self.w, "  {dim}{label:<8}{reset}  {value}")
    }

    /// A `systemctl`-style health headline followed by a blank line.
    pub fn status_block(&mut self, health: Health, headline: &str) -> std::io::Result<()> {
        let Theme {
            green,
            dim,
            bold,
            reset,
            ..
        } = self.theme;
        let glyph = match health {
            Health::Up => format!("{green}●{reset}"),
            Health::Down => format!("{dim}○{reset}"),
            Health::Unknown => format!("{dim}●{reset}"),
        };
        writeln!(self.w, "{glyph} {bold}{headline}{reset}")?;
        self.blank()
    }

    /// A dim, indented secondary note (e.g. a follow-up suggestion).
    pub fn note(&mut self, s: &str) -> std::io::Result<()> {
        let (dim, reset) = (self.theme.dim, self.theme.reset);
        writeln!(self.w, "  {dim}{s}{reset}")
    }

    /// The `error:` headline of an error block (red, bold).
    pub fn error_head(&mut self, s: &str) -> std::io::Result<()> {
        let (red, bold, reset) = (self.theme.red, self.theme.bold, self.theme.reset);
        writeln!(self.w, "{red}{bold}error:{reset} {s}")
    }

    /// A `while:` context layer of an error block (dim).
    pub fn while_line(&mut self, s: &str) -> std::io::Result<()> {
        let (dim, reset) = (self.theme.dim, self.theme.reset);
        writeln!(self.w, "  {dim}while:{reset} {s}")
    }

    /// A `hint:` remediation line of an error block (cyan).
    pub fn hint_line(&mut self, s: &str) -> std::io::Result<()> {
        let (cyan, reset) = (self.theme.cyan, self.theme.reset);
        writeln!(self.w, "  {cyan}hint:{reset} {s}")
    }

    /// Render a [`Table`] — header row then data rows, columns auto-sized.
    pub fn table(&mut self, t: &Table) -> std::io::Result<()> {
        let widths = t.widths();
        self.row_cells(&t.headers, &widths)?;
        for row in &t.rows {
            self.row_cells(row, &widths)?;
        }
        Ok(())
    }

    fn row_cells(&mut self, cells: &[String], widths: &[usize]) -> std::io::Result<()> {
        let last = cells.len().saturating_sub(1);
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i == last {
                line.push_str(cell);
            } else {
                let pad = widths.get(i).copied().unwrap_or(0);
                line.push_str(&format!("{cell:<pad$}  "));
            }
        }
        writeln!(self.w, "{}", line.trim_end())
    }

    /// Pretty-print an embedded JSON value, each line indented by two spaces.
    pub fn embedded_json(&mut self, v: &serde_json::Value) -> std::io::Result<()> {
        let pretty = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
        for line in pretty.lines() {
            writeln!(self.w, "  {line}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(f: impl FnOnce(&mut Human)) -> String {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut h = Human::new(&mut buf, Theme::none());
            f(&mut h);
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn status_block_up_renders_bullet_headline_and_field() {
        let s = render(|h| {
            h.status_block(Health::Up, "bitrouter is running").unwrap();
            h.field("pid", "123").unwrap();
        });
        assert!(s.contains("● bitrouter is running"), "{s:?}");
        assert!(s.contains("  pid       123"), "{s:?}");
    }

    #[test]
    fn status_block_down_uses_hollow_glyph() {
        let s = render(|h| {
            h.status_block(Health::Down, "bitrouter is stopped")
                .unwrap()
        });
        assert!(s.contains("○ bitrouter is stopped"), "{s:?}");
    }

    #[test]
    fn table_pads_columns_and_trims_last() {
        let t = Table::new(["ID", "MODELS"])
            .row(["openai", "42"])
            .row(["x", "1"]);
        let s = render(|h| h.table(&t).unwrap());
        assert!(s.starts_with("ID      MODELS"), "{s:?}");
        assert!(s.contains("openai  42"), "{s:?}");
        // last column trimmed — no trailing spaces
        assert!(s.lines().all(|l| l == l.trim_end()), "{s:?}");
    }

    #[test]
    fn error_block_heads() {
        let s = render(|h| {
            h.error_head("boom").unwrap();
            h.while_line("loading x").unwrap();
            h.hint_line("try y").unwrap();
        });
        assert_eq!(s, "error: boom\n  while: loading x\n  hint: try y\n");
    }
}
