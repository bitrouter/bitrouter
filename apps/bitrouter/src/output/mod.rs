//! CLI output layer.
//!
//! Every CLI command computes a strongly-typed **report** that implements
//! [`CliReport`] — it is both `Serialize` (the JSON view) and renderable to a
//! human view via [`Human`]. The [`Output`] driver is the single
//! place that writes the result to stdout, picking the format from the global
//! `--json` / `--human` flags.
//!
//! Default is JSON (agent-native). Diagnostics never come through here — they
//! go to stderr — so `bitrouter <cmd> 2>/dev/null | jq` always sees one clean
//! JSON value.

pub mod error;
pub mod human;
pub mod reports;

use std::io::Write;

use human::{Human, Theme};

/// The output format for a command's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// One pretty-printed JSON value (the default).
    Json,
    /// The human-readable rendering.
    Human,
}

/// A command result that can be emitted as JSON or as a human view.
///
/// The `erased_serde::Serialize` supertrait lets a `&dyn CliReport` be
/// serialized (via [`serialize_trait_object!`](erased_serde::serialize_trait_object))
/// while each concrete report just derives `serde::Serialize` — so adding a
/// command that returns output is impossible without supplying both views.
pub trait CliReport: erased_serde::Serialize {
    /// Render the human view using the shared components.
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()>;

    /// The process exit code for this result. Defaults to success; a few
    /// reports (validation failures, error envelopes) override it.
    fn exit_code(&self) -> i32 {
        0
    }
}

erased_serde::serialize_trait_object!(CliReport);

/// The single stdout writer. Constructed once from the global flags.
pub struct Output {
    fmt: Format,
}

impl Output {
    /// Construct with an explicit format.
    pub fn new(fmt: Format) -> Self {
        Self { fmt }
    }

    /// Pick the format from the global flags. `--human` selects the human view;
    /// `--json` (or neither) is JSON. The two are mutually exclusive at the
    /// clap layer; the `&& !json` is belt-and-suspenders.
    pub fn from_flags(json: bool, human: bool) -> Self {
        Self::new(if human && !json {
            Format::Human
        } else {
            Format::Json
        })
    }

    /// The active output format.
    pub fn format(&self) -> Format {
        self.fmt
    }

    /// Emit a report to stdout in the active format.
    pub fn emit(&self, r: &dyn CliReport) -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        self.write(&mut out, Theme::for_stdout(), r)
    }

    /// Render to a buffer with an empty palette — the testable seam.
    pub fn render_to_vec(&self, r: &dyn CliReport) -> Vec<u8> {
        let mut v = Vec::new();
        self.write(&mut v, Theme::none(), r)
            .expect("render to vec is infallible");
        v
    }

    fn write(&self, out: &mut dyn Write, theme: Theme, r: &dyn CliReport) -> std::io::Result<()> {
        match self.fmt {
            Format::Json => {
                serde_json::to_writer_pretty(&mut *out, r).map_err(std::io::Error::other)?;
                writeln!(out)
            }
            Format::Human => r.render(&mut Human::new(&mut *out, theme)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize)]
    struct Dummy {
        ok: bool,
    }

    impl CliReport for Dummy {
        fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
            h.line(&format!("ok={}", self.ok))
        }
    }

    #[test]
    fn emit_json_writes_pretty_object() {
        let buf = Output::new(Format::Json).render_to_vec(&Dummy { ok: true });
        assert_eq!(String::from_utf8(buf).unwrap(), "{\n  \"ok\": true\n}\n");
    }

    #[test]
    fn emit_human_uses_render() {
        let buf = Output::new(Format::Human).render_to_vec(&Dummy { ok: false });
        assert_eq!(String::from_utf8(buf).unwrap(), "ok=false\n");
    }
}
