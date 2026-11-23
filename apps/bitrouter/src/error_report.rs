//! CLI notices and shared error classification hints.

use std::io::Write;

use crate::commands::invocation;

use crate::style::Palette;

/// Print an informational notice for CLI setup and startup.
pub fn info(message: impl std::fmt::Display) {
    let palette = Palette::for_stderr();
    let mut stderr = std::io::stderr().lock();
    let _ = write_info(&mut stderr, &palette, message);
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
        return Some(format!(
            "Drop `-c <path>` to use the default resolution order, or run\n\
             `{cli} init -c <path>` to write a starter config there.",
            cli = invocation::name()
        ));
    }
    // BITROUTER_HOME set but file missing.
    if root.contains("BITROUTER_HOME is set to") {
        return Some(format!(
            "Unset `BITROUTER_HOME`, or run\n\
             `{cli} init -c $BITROUTER_HOME/bitrouter.yaml` to scaffold one.",
            cli = invocation::name()
        ));
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
    fn write_info_uses_info_prefix() {
        let palette = Palette::none();
        let mut buf: Vec<u8> = Vec::new();
        write_info(&mut buf, &palette, "scaffolded a thing").unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out, "info: scaffolded a thing\n");
    }
}
