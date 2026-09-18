//! Shared CLI and foreground-host log-filter resolution.

/// The filter used when neither `RUST_LOG` nor `server.log_level` supplies a
/// usable one.
const DEFAULT_LOG_FILTER: &str = "info";

/// Resolve the tracing filter. Precedence, highest first:
///
/// 1. **`RUST_LOG`** — the Rust convention, and the escape hatch an operator
///    reaches for during an incident without editing (and reloading) config.
/// 2. **`server.log_level`** from `bitrouter.yaml`. Only `serve` has a config
///    loaded this early; every other command passes `None`, because its
///    subscriber is installed before any config is read.
/// 3. **`info`**.
///
/// Returns the filter plus an optional warning. Nothing can be logged before
/// the subscriber this feeds is installed, so an unparseable filter string is
/// handed back for the caller to emit *after* `init()` rather than silently
/// swallowed — the same deferred-diagnostic shape `serve` already uses for
/// OTel init errors.
pub(crate) fn resolve_env_filter(
    config_log_level: Option<&str>,
) -> (tracing_subscriber::EnvFilter, Option<String>) {
    // `EnvFilter::try_from_default_env` collapses "unset" and "set but
    // invalid" into one `Err`, which is exactly the distinction that decides
    // whether the config value gets a turn — so read the variable directly.
    let rust_log = std::env::var(tracing_subscriber::EnvFilter::DEFAULT_ENV).ok();
    resolve_env_filter_from(rust_log.as_deref(), config_log_level)
}

/// The precedence logic behind [`resolve_env_filter`], with the environment
/// passed in so it is testable without mutating process-global state.
fn resolve_env_filter_from(
    rust_log: Option<&str>,
    config_log_level: Option<&str>,
) -> (tracing_subscriber::EnvFilter, Option<String>) {
    let non_blank = |s: &&str| !s.trim().is_empty();
    let (source, raw) = match (
        rust_log.filter(non_blank),
        config_log_level.filter(non_blank),
    ) {
        (Some(raw), _) => (tracing_subscriber::EnvFilter::DEFAULT_ENV, raw),
        (None, Some(level)) => ("server.log_level", level),
        (None, None) => return (tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER), None),
    };
    match parse_log_filter(raw) {
        Ok(filter) => (filter, None),
        Err(reason) => (
            tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER),
            Some(format!(
                "invalid {source} value {raw:?}: {reason} — falling back to `{DEFAULT_LOG_FILTER}`"
            )),
        ),
    }
}

/// Parse one filter string, rejecting the failure mode `EnvFilter` itself
/// won't.
///
/// `EnvFilter::try_new("dbug")` **succeeds**: with no directive syntax present
/// it reads the word as a *target* named `dbug` at trace level. The resulting
/// filter matches nothing, so a one-character typo in `log_level` silently
/// mutes the daemon instead of erroring — the worst possible outcome for a
/// logging setting. So a bare word (no `=`, no `,`) must parse as a level;
/// anything carrying directive syntax is handed to `EnvFilter` as-is.
fn parse_log_filter(raw: &str) -> std::result::Result<tracing_subscriber::EnvFilter, String> {
    let raw = raw.trim();
    if !raw.contains('=')
        && !raw.contains(',')
        && raw
            .parse::<tracing_subscriber::filter::LevelFilter>()
            .is_err()
    {
        return Err(
            "expected a level (`trace`, `debug`, `info`, `warn`, `error`, `off`) or an \
             `EnvFilter` directive list such as `info,bitrouter=debug`"
                .to_string(),
        );
    }
    tracing_subscriber::EnvFilter::try_new(raw).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    // ===== tracing filter resolution =====

    #[test]
    fn config_log_level_is_used_when_rust_log_is_unset() {
        let (filter, warning) = resolve_env_filter_from(None, Some("debug"));
        assert_eq!(filter.to_string(), "debug");
        assert!(warning.is_none());
    }

    #[test]
    fn config_log_level_accepts_per_target_filter_syntax() {
        let (filter, warning) = resolve_env_filter_from(None, Some("warn,bitrouter=trace"));
        // `EnvFilter`'s `Display` does not preserve directive order.
        let rendered = filter.to_string();
        assert!(rendered.contains("warn"), "{rendered}");
        assert!(rendered.contains("bitrouter=trace"), "{rendered}");
        assert!(warning.is_none());
    }

    #[test]
    fn rust_log_wins_over_config_log_level() {
        let (filter, warning) = resolve_env_filter_from(Some("trace"), Some("error"));
        assert_eq!(filter.to_string(), "trace");
        assert!(warning.is_none());
    }

    #[test]
    fn defaults_to_info_when_neither_source_is_set() {
        let (filter, warning) = resolve_env_filter_from(None, None);
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        assert!(warning.is_none());
    }

    #[test]
    fn blank_sources_fall_through_rather_than_erroring() {
        // An empty `RUST_LOG=` must not shadow a real config value, and a
        // blank `log_level: ""` must not be treated as a filter.
        let (filter, warning) = resolve_env_filter_from(Some("  "), Some("debug"));
        assert_eq!(filter.to_string(), "debug");
        assert!(warning.is_none());

        let (filter, warning) = resolve_env_filter_from(None, Some(""));
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        assert!(warning.is_none());
    }

    #[test]
    fn invalid_config_log_level_warns_and_falls_back() -> anyhow::Result<()> {
        let (filter, warning) = resolve_env_filter_from(None, Some("not-a-level"));
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        let warning =
            warning.ok_or_else(|| anyhow::anyhow!("an unparseable log_level must be reported"))?;
        assert!(warning.contains("server.log_level"), "{warning}");
        assert!(warning.contains("not-a-level"), "{warning}");
        Ok(())
    }

    #[test]
    fn typo_level_does_not_silently_mute_the_daemon() -> anyhow::Result<()> {
        // Regression guard: `EnvFilter::try_new("dbug")` succeeds, reading the
        // word as a *target* at trace level — a filter that matches nothing.
        // Left unchecked, `log_level: dbug` would silence the daemon with no
        // diagnostic at all.
        assert_eq!(
            tracing_subscriber::EnvFilter::try_new("dbug")?.to_string(),
            "dbug=trace",
        );

        let (filter, warning) = resolve_env_filter_from(None, Some("dbug"));
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        assert!(warning.is_some(), "a typo'd level must warn, not mute");
        Ok(())
    }

    #[test]
    fn every_level_name_is_accepted() {
        for level in ["trace", "debug", "info", "warn", "error", "off"] {
            let (filter, warning) = resolve_env_filter_from(None, Some(level));
            assert!(warning.is_none(), "{level} should be valid: {warning:?}");
            assert_eq!(filter.to_string(), level);
        }
    }

    #[test]
    fn invalid_rust_log_warns_and_does_not_fall_back_to_config() -> anyhow::Result<()> {
        // `RUST_LOG` is an explicit operator override; a typo in it should be
        // reported rather than silently resolved from config behind the
        // operator's back.
        let (filter, warning) = resolve_env_filter_from(Some("not-a-level"), Some("debug"));
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        let warning =
            warning.ok_or_else(|| anyhow::anyhow!("an unparseable RUST_LOG must be reported"))?;
        assert!(warning.contains("RUST_LOG"), "{warning}");
        Ok(())
    }

    #[test]
    fn server_config_default_log_level_is_a_valid_filter() {
        // The default that ships in `ServerConfig` (and the JSON Schema) must
        // survive the same parse path a user-supplied value takes.
        let default_level = bitrouter_sdk::config::ServerConfig::default().log_level;
        let (filter, warning) = resolve_env_filter_from(None, Some(&default_level));
        assert_eq!(filter.to_string(), DEFAULT_LOG_FILTER);
        assert!(warning.is_none());
    }
}
