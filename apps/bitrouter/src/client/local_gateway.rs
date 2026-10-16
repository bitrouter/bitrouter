//! Local gateway endpoint selection and best-effort startup shared by setup and launch.

use crate::commands::invocation;
use crate::style::Palette;

/// BitRouter's own API-key env var (`brk_…`). When set, it is forwarded to the
/// harness as the gateway bearer credential.
pub(crate) const BITROUTER_API_KEY_ENV: &str = "BITROUTER_API_KEY";

/// Placeholder credential injected when no real key is available. Ignored by
/// the daemon under `skip_auth: true` (the `bro init` default); the
/// harness merely needs *some* credential to start.
pub(crate) const PLACEHOLDER_API_KEY: &str = "bitrouter-local";

/// Read an environment variable, treating an unset *or empty* value as absent.
pub(crate) fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Derive the client-facing base URL from the daemon's `server.listen`
/// (`host:port`). Wildcard bind addresses are rewritten to loopback because a
/// client cannot *connect* to `0.0.0.0` / `::` — those mean "bind every
/// interface", not "reach me here".
pub(crate) fn derive_base_url(listen: &str) -> String {
    let (host, port) = split_listen(listen);
    format!("http://{}:{}", rewrite_host(host), port)
}

/// The default daemon port (mirrors `ServerConfig::default().listen`). Used
/// when `server.listen` carries a bare host with no `:port`.
const DEFAULT_PORT: &str = "4356";

/// Split a `server.listen` value into `(host, port)`, defaulting the port when
/// absent. Handles bracketed IPv6 (`[::1]:4356`, `[::1]`) so the `rsplit_once`
/// does not mistake a colon *inside* the brackets for the port separator.
fn split_listen(listen: &str) -> (&str, &str) {
    // Bracketed IPv6: the port (if any) follows the closing bracket.
    if listen.starts_with('[') {
        return match listen.rsplit_once("]:") {
            Some((host, port)) => (&listen[..host.len() + 1], port),
            // `[::1]` with no port.
            None => (listen, DEFAULT_PORT),
        };
    }
    match listen.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => (listen, DEFAULT_PORT),
    }
}

/// Rewrite a wildcard bind host to its loopback equivalent for a *client*
/// connection. `0.0.0.0` / empty → `127.0.0.1`; `::` / `[::]` → `[::1]`.
fn rewrite_host(host: &str) -> &str {
    match host {
        "0.0.0.0" | "" => "127.0.0.1",
        "::" | "[::]" => "[::1]",
        other => other,
    }
}

/// True when `listen` binds a loopback / wildcard address — i.e. a daemon on
/// *this* host that `bro spawn` may auto-start. A remote or LAN host is
/// someone else's daemon, which we can only warn about. Exact-match only:
/// `127.0.0.0/8` aliases (e.g. `127.0.0.2`) and IPv4-mapped IPv6 fall through to
/// the warn path — the fail-safe direction (never a wrong auto-start).
#[cfg(any(feature = "harness", test))]
pub(crate) fn listen_is_local(listen: &str) -> bool {
    let (host, _port) = split_listen(listen);
    matches!(
        host,
        "127.0.0.1" | "0.0.0.0" | "" | "::1" | "[::1]" | "::" | "[::]" | "localhost"
    )
}

/// Extract the `host[:port]` authority from a base URL for a best-effort
/// reachability note. Returns `None` when there is no authority to probe.
#[cfg(any(feature = "harness", test))]
pub(crate) fn listen_from_base_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or(rest);
    (!authority.is_empty()).then(|| authority.to_string())
}

/// Ensure the local BitRouter daemon is up before launching the agent. Probes
/// the control socket; when nothing is listening (and `--no-start` was not
/// given) it prints a hint and auto-starts a detached `serve`, waiting for
/// readiness. Best-effort throughout: on any failure it warns and returns so
/// the agent still launches (and surfaces its own connection error) — matching
/// spawn's "never block the launch" stance.
pub(crate) async fn ensure_local_daemon(
    source: &crate::paths::ConfigSource,
    cfg: &crate::config::Config,
    no_start: bool,
) {
    let socket = crate::daemon::socket_path_for(source, cfg);
    match crate::daemon::probe_status(&socket).await {
        // Already running — nothing to do.
        Ok(Some(_)) => {}
        // Definitively not reachable — auto-start unless opted out.
        Ok(None) => {
            let p = Palette::for_stderr();
            if no_start {
                warn_if_daemon_unreachable(&cfg.server.listen);
                return;
            }
            eprintln!(
                "{cyan}note:{reset} no BitRouter daemon is running — starting one…",
                cyan = p.cyan,
                reset = p.reset,
            );
            let log_path = source.home().join("bitrouter.log");
            match crate::daemon::start_and_wait(
                source,
                &log_path,
                Some(&socket),
                crate::daemon::DAEMON_READY_TIMEOUT,
            )
            .await
            {
                Ok(crate::daemon::DaemonStartOutcome::Ready(info)) => {
                    eprintln!(
                        "{cyan}note:{reset} BitRouter daemon ready (pid {})",
                        info.pid,
                        cyan = p.cyan,
                        reset = p.reset,
                    );
                }
                Ok(crate::daemon::DaemonStartOutcome::NotReadyInTime { pid }) => {
                    eprintln!(
                        "{cyan}note:{reset} daemon started (pid {pid}) but is not ready yet — \
                         the agent may need a moment; logs at {}",
                        log_path.display(),
                        cyan = p.cyan,
                        reset = p.reset,
                    );
                }
                Ok(crate::daemon::DaemonStartOutcome::Exited { status, log_tail }) => {
                    eprintln!(
                        "{cyan}note:{reset} daemon exited during startup ({status}) — \
                         launching the agent anyway",
                        cyan = p.cyan,
                        reset = p.reset,
                    );
                    crate::daemon::eprint_failure_log(&log_path, &log_tail);
                }
                Err(e) => {
                    eprintln!(
                        "{cyan}note:{reset} could not start the daemon ({e:#}) — \
                         launching the agent anyway",
                        cyan = p.cyan,
                        reset = p.reset,
                    );
                }
            }
        }
        // Reachable but the exchange errored — assume it's up; don't double-start.
        Err(e) => {
            tracing::debug!(error = %e, "daemon status probe errored; assuming up");
        }
    }
}

/// Best-effort TCP reachability probe against the daemon's listen address.
/// Prints a one-line warning when nothing is listening; never errors.
pub(crate) fn warn_if_daemon_unreachable(listen: &str) {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    // Map the wildcard bind host to loopback for the *connect* attempt, same
    // as the base-URL derivation.
    let (host, port) = split_listen(listen);
    let probe = format!("{}:{}", rewrite_host(host), port);

    let reachable = probe
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .map(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok())
        .unwrap_or(false);

    if !reachable {
        let p = Palette::for_stderr();
        eprintln!(
            "{cyan}note:{reset} no BitRouter daemon appears to be listening on {probe} — \
             start one with `{cli} start` (the agent will fail to reach it otherwise).",
            cyan = p.cyan,
            reset = p.reset,
            cli = invocation::name(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_is_local_classifies_loopback_and_wildcard() {
        for local in [
            "127.0.0.1:4356",
            "0.0.0.0:4356",
            "[::1]:4356",
            "[::]:4356",
            "localhost:4356",
            "127.0.0.1",
        ] {
            assert!(listen_is_local(local), "{local} should be local");
        }
        for remote in [
            "router.internal:8080",
            "192.168.1.5:4356",
            "10.0.0.3:4356",
            "example.com:443",
        ] {
            assert!(!listen_is_local(remote), "{remote} should be remote");
        }
    }

    #[test]
    fn listen_from_base_url_extracts_authority() {
        assert_eq!(
            listen_from_base_url("http://127.0.0.1:4356").as_deref(),
            Some("127.0.0.1:4356")
        );
        assert_eq!(
            listen_from_base_url("https://router.example.com/v1").as_deref(),
            Some("router.example.com")
        );
        // No scheme → treated as a bare authority.
        assert_eq!(
            listen_from_base_url("127.0.0.1:4356").as_deref(),
            Some("127.0.0.1:4356")
        );
        assert_eq!(listen_from_base_url(""), None);
    }

    #[test]
    fn base_url_rewrites_wildcard_bind_to_loopback() {
        assert_eq!(derive_base_url("0.0.0.0:4356"), "http://127.0.0.1:4356");
        assert_eq!(derive_base_url("[::]:4356"), "http://[::1]:4356");
    }

    #[test]
    fn base_url_preserves_explicit_host() {
        assert_eq!(derive_base_url("127.0.0.1:4356"), "http://127.0.0.1:4356");
        assert_eq!(
            derive_base_url("router.internal:8080"),
            "http://router.internal:8080"
        );
        // A bracketed IPv6 literal keeps its brackets and port.
        assert_eq!(derive_base_url("[::1]:9000"), "http://[::1]:9000");
    }

    #[test]
    fn base_url_defaults_port_when_missing() {
        assert_eq!(derive_base_url("127.0.0.1"), "http://127.0.0.1:4356");
        // Bracketed IPv6 without a port must not split inside the brackets.
        assert_eq!(derive_base_url("[::1]"), "http://[::1]:4356");
    }
}
