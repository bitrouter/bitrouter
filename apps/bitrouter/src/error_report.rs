//! User-facing CLI diagnostics — replaces anyhow's default
//! `Debug` rendering (`"Error: …\n\nCaused by:\n    …"`) with a tighter,
//! actionable presentation.
//!
//! Shape of an error report:
//!
//! ```text
//! error: <root-cause message, with HTTP-taxonomy prefix stripped>
//!   while: <first context layer>            (one line per layer, oldest-to-newest)
//!   while: …
//!
//!   hint: <how to fix, if we recognise the failure mode>
//!        <…wrapped to subsequent lines>
//! ```
//!
//! Shape of a non-error notice (e.g. the first-run scaffold message):
//!
//! ```text
//! info: <one-liner>
//! ```
//!
//! Colours: red for `error:`, cyan for `info:`, dim for `while:` / `hint:`,
//! emitted only when stderr is a TTY (so piping to a log file stays clean).

use std::io::Write;

use crate::style::Palette;

/// Print an anyhow error chain to stderr in the documented CLI shape.
/// Caller decides whether to `std::process::exit(1)` afterwards.
pub fn report(err: &anyhow::Error) {
    let palette = Palette::for_stderr();
    let mut stderr = std::io::stderr().lock();
    // I/O failure writing to stderr is unrecoverable; ignore.
    let _ = write_report(&mut stderr, &palette, err);
}

/// Print an informational notice in the same visual family as
/// [`report`]. Used by paths.rs's first-run scaffold message and any
/// future "we did a thing on your behalf" surface.
pub fn info(message: impl std::fmt::Display) {
    let palette = Palette::for_stderr();
    let mut stderr = std::io::stderr().lock();
    let _ = write_info(&mut stderr, &palette, message);
}

/// Render the report to an arbitrary writer with an explicit palette.
/// Pulled out from [`report`] so tests can capture and assert on the
/// rendered bytes without touching stderr.
fn write_report(out: &mut dyn Write, p: &Palette, err: &anyhow::Error) -> std::io::Result<()> {
    let chain: Vec<String> = err.chain().map(|e| e.to_string()).collect();
    // The root cause is the most actionable part — surface it as the
    // headline. Anyhow's chain runs head→root; we want the tail.
    // `chain()` is documented as non-empty (the head is the error itself),
    // but fall back rather than panic if that ever changes upstream.
    let root_raw = chain
        .last()
        .map(String::as_str)
        .unwrap_or("(unknown error)");
    let root = strip_status_prefix(root_raw);
    writeln!(
        out,
        "{red}{bold}error:{reset} {root}",
        red = p.red,
        bold = p.bold,
        reset = p.reset
    )?;

    // Intermediate context layers, oldest to newest, skipping the root.
    // Anyhow's chain is head→root; the head is usually the outermost
    // `.context("loading X")`, which is the most user-facing locator.
    if chain.len() > 1 {
        for layer in chain[..chain.len() - 1].iter() {
            let stripped = strip_status_prefix(layer);
            writeln!(
                out,
                "  {dim}while:{reset} {stripped}",
                dim = p.dim,
                reset = p.reset
            )?;
        }
    }

    if let Some(hint) = hint_for(root) {
        writeln!(out)?;
        for line in hint.lines() {
            writeln!(
                out,
                "  {cyan}hint:{reset} {line}",
                cyan = p.cyan,
                reset = p.reset
            )?;
        }
    }
    Ok(())
}

fn write_info(
    out: &mut dyn Write,
    p: &Palette,
    message: impl std::fmt::Display,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{cyan}{bold}info:{reset} {message}",
        cyan = p.cyan,
        bold = p.bold,
        reset = p.reset
    )
}

/// `BitrouterError`'s `Display` impl carries an HTTP-shape prefix on
/// the variants with a payload (`"bad request: …"`, `"internal error: …"`,
/// etc.) — the CLI doesn't need them. Strip the prefix when we recognise
/// one; leave the message alone otherwise. Keeps the formatter lossless
/// for any error type outside our taxonomy.
///
/// The payload-less variants (`RateLimited` → `"rate limited"`,
/// `UpstreamTimeout` → `"upstream timeout"`) are not stripped — they're
/// already short, and they shouldn't reach the CLI surface anyway (those
/// failures are produced by HTTP request handlers, not by the
/// CLI-driven assembly / config-loading paths).
///
/// `Upstream { status, message }` renders as `"upstream error (502): …"`;
/// the status is genuine debugging signal so we preserve the whole
/// string rather than stripping a prefix.
pub(crate) fn strip_status_prefix(msg: &str) -> &str {
    const PREFIXES: &[&str] = &[
        "bad request: ",
        "internal error: ",
        "unauthorized: ",
        "forbidden: ",
        "payment required: ",
        "not found: ",
    ];
    for prefix in PREFIXES {
        if let Some(rest) = msg.strip_prefix(prefix) {
            return rest;
        }
    }
    msg
}

/// Recognise a handful of common failure modes and emit an actionable
/// next-step hint. Matches by anchored prefix or specific substring
/// against the *stripped* root message so display-prefix churn doesn't
/// break the table.
pub(crate) fn hint_for(root: &str) -> Option<String> {
    // Undefined config env-var. Pull the var name out so the hint can
    // name it explicitly.
    if let Some(rest) = root.strip_prefix("config references undefined environment variable '")
        && let Some(var) = rest.strip_suffix("'")
    {
        return Some(format!(
            "Set `{var}` in your environment (e.g. `export {var}=…`),\n\
                 or remove the `${{{var}}}` reference from bitrouter.yaml."
        ));
    }
    // `-c <missing>` user error.
    if root.contains("does not exist (passed via -c)") {
        return Some(
            "Drop `-c <path>` to use the default resolution order, or run\n\
             `bitrouter init -c <path>` to write a starter config there."
                .into(),
        );
    }
    // BITROUTER_HOME set but file missing.
    if root.contains("BITROUTER_HOME is set to") {
        return Some(
            "Unset `BITROUTER_HOME`, or run\n\
             `bitrouter init -c $BITROUTER_HOME/bitrouter.yaml` to scaffold one."
                .into(),
        );
    }
    // The database wouldn't open. Match the specific phrasing from
    // `BitrouterError::internal(format!("connecting to database {url}: …"))`
    // in `apps/bitrouter/src/db/mod.rs` — broad substring matching against
    // bare "sqlite" would false-positive on unrelated migration /
    // policy-store messages.
    if root.contains("connecting to database") {
        return Some(
            "Check the `database.url` value in bitrouter.yaml. sqlite, postgres\n\
             and mysql URLs are all supported; `sqlite://./bitrouter.db` is the\n\
             default and the file is created on first run."
                .into(),
        );
    }
    // No `$HOME` set (rare; happens in some CI / container shells).
    if root.contains("could not determine home directory") {
        return Some(
            "Either set `BITROUTER_HOME=<dir>` (with a `bitrouter.yaml` inside),\n\
             or pass `-c <path>` explicitly."
                .into(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(err: &anyhow::Error) -> String {
        let palette = Palette::none();
        let mut buf: Vec<u8> = Vec::new();
        write_report(&mut buf, &palette, err).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn strip_known_status_prefixes() {
        assert_eq!(strip_status_prefix("bad request: foo"), "foo");
        assert_eq!(strip_status_prefix("internal error: boom"), "boom");
        assert_eq!(strip_status_prefix("not found: route"), "route");
        // Unknown prefix → leave alone.
        assert_eq!(strip_status_prefix("loading /tmp/x"), "loading /tmp/x");
        // Upstream is intentionally preserved — the status is signal.
        assert_eq!(
            strip_status_prefix("upstream error (502): boom"),
            "upstream error (502): boom"
        );
        // Bare "rate limited" / "upstream timeout" pass through (they
        // don't have the ": " suffix the table strips on).
        assert_eq!(strip_status_prefix("rate limited"), "rate limited");
        assert_eq!(strip_status_prefix("upstream timeout"), "upstream timeout");
    }

    #[test]
    fn hint_extracts_undefined_env_var_name() {
        let hint = hint_for("config references undefined environment variable 'OPENAI_API_KEY'")
            .expect("hint produced");
        assert!(hint.contains("OPENAI_API_KEY"));
        assert!(hint.contains("export OPENAI_API_KEY"));
    }

    #[test]
    fn hint_recognises_passed_via_dash_c() {
        let hint = hint_for("config file '/x.yaml' does not exist (passed via -c). foo").unwrap();
        assert!(hint.contains("-c <path>"));
    }

    #[test]
    fn hint_recognises_bitrouter_home_missing_file() {
        let hint =
            hint_for("BITROUTER_HOME is set to '/x' but 'bitrouter.yaml' is missing there. foo")
                .unwrap();
        assert!(hint.contains("BITROUTER_HOME"));
    }

    #[test]
    fn hint_does_not_fire_for_unrelated_sqlite_mentions() {
        // Bare "sqlite" mentions (e.g. an internal-error from
        // policy-store migrations) must not pull in the database.url
        // hint — only the specific "connecting to database" phrase does.
        assert!(hint_for("internal error: failed to apply sqlite migrations").is_none());
    }

    #[test]
    fn hint_returns_none_for_unknown_messages() {
        assert!(hint_for("something we have no opinion on").is_none());
    }

    #[test]
    fn report_renders_chain_head_to_root_in_order() {
        let err: anyhow::Error = anyhow::anyhow!("bad request: config thing exploded")
            .context("loading /tmp/bitrouter.yaml")
            .context("preparing models command");
        let out = render(&err);
        // Root cause becomes the `error:` headline with prefix stripped.
        assert!(
            out.contains("error: config thing exploded"),
            "missing root headline: {out}"
        );
        // Outer contexts render as `while:` lines, prefix-stripped if applicable.
        assert!(
            out.contains("while: preparing models command"),
            "missing outer context: {out}"
        );
        assert!(
            out.contains("while: loading /tmp/bitrouter.yaml"),
            "missing inner context: {out}"
        );
        // Order: head → root (so outer command appears before the
        // inner `loading …` context).
        let outer = out.find("preparing models command").unwrap();
        let inner = out.find("loading /tmp/bitrouter.yaml").unwrap();
        assert!(outer < inner, "context order wrong:\n{out}");
    }

    #[test]
    fn report_appends_hint_block_when_pattern_matches() {
        let err: anyhow::Error =
            anyhow::anyhow!("config references undefined environment variable 'FOO_KEY'")
                .context("loading /tmp/bitrouter.yaml");
        let out = render(&err);
        assert!(
            out.contains("hint: Set `FOO_KEY`"),
            "no hint emitted:\n{out}"
        );
    }

    #[test]
    fn report_omits_hint_block_when_no_pattern_matches() {
        let err: anyhow::Error = anyhow::anyhow!("something obscure happened");
        let out = render(&err);
        assert!(!out.contains("hint:"), "spurious hint emitted:\n{out}");
    }

    #[test]
    fn write_info_uses_info_prefix() {
        let palette = Palette::none();
        let mut buf: Vec<u8> = Vec::new();
        write_info(&mut buf, &palette, "scaffolded a thing").unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out, "info: scaffolded a thing\n");
    }
}
