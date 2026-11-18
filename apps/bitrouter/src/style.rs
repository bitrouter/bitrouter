//! Shared terminal styling — ANSI palette + TTY / `NO_COLOR` gating —
//! used by every user-facing CLI surface (error reports, status
//! reports, future report-style output). Centralising the palette and
//! the TTY-detection logic keeps the look consistent across surfaces
//! and means a future "force colour" / theme toggle only has to change
//! one place.

use std::io::IsTerminal;

/// ANSI escape strings, picked once per call to [`Palette::for_stderr`]
/// / [`Palette::for_stdout`]. When colour is suppressed (stream is not
/// a TTY, or `NO_COLOR` is set) every field is `""` so format strings
/// stay valid but emit nothing.
pub struct Palette {
    /// Red. Used for the `error:` head and the `●` stopped indicator.
    pub red: &'static str,
    /// Green. Used for the `●` running indicator.
    pub green: &'static str,
    /// Cyan. Used for the `info:` / `hint:` heads.
    pub cyan: &'static str,
    /// Bold. Used to lift the state word ("running" / "stopped") and
    /// the `error:` head.
    pub bold: &'static str,
    /// Dim (faint). Used for secondary labels (`while:`, `pid`, …) and
    /// the hint sentence at the bottom of a stopped-status report.
    pub dim: &'static str,
    /// Reset to default attributes. Emit after every styled run so the
    /// next text doesn't inherit colour.
    pub reset: &'static str,
}

impl Palette {
    /// Full ANSI palette — emit on TTYs without `NO_COLOR`.
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

    /// Empty palette — every field is `""`. Used when stdout/stderr is
    /// redirected, `NO_COLOR` is set, or in unit tests.
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

    /// Pick the palette appropriate for stderr (used by error/info
    /// surfaces). Honours `NO_COLOR` (<https://no-color.org/>).
    pub fn for_stderr() -> Self {
        if no_color() || !std::io::stderr().is_terminal() {
            Self::none()
        } else {
            Self::ansi()
        }
    }

    /// Pick the palette appropriate for stdout (used by status / other
    /// structured-report surfaces). Piping `bitrouter status > file`
    /// strips colour automatically via [`IsTerminal`].
    pub fn for_stdout() -> Self {
        if no_color() || !std::io::stdout().is_terminal() {
            Self::none()
        } else {
            Self::ansi()
        }
    }
}

/// True when `NO_COLOR` is set to a non-empty value. The variable's
/// presence (even empty) suggesting opt-out is the older draft of the
/// convention; the modern reading (no-color.org) is that an empty value
/// has no effect.
fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_and_none_have_matching_field_shapes() {
        let a = Palette::ansi();
        let n = Palette::none();
        // Every styled field has a non-empty escape under `ansi` and an
        // empty string under `none` — that asymmetry is the whole point.
        for (field_a, field_n) in [
            (a.red, n.red),
            (a.green, n.green),
            (a.cyan, n.cyan),
            (a.bold, n.bold),
            (a.dim, n.dim),
            (a.reset, n.reset),
        ] {
            assert!(!field_a.is_empty(), "ansi field unexpectedly empty");
            assert!(field_n.is_empty(), "none field unexpectedly populated");
        }
    }
}
