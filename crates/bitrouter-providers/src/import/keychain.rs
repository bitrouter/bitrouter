//! macOS login-Keychain reader via the `security(1)` CLI.
//!
//! We shell out to `security` rather than linking a Keychain crate: it is the
//! same mechanism the vendor CLIs and the OpenClaw reference
//! (`src/agents/cli-credentials.ts`) use, it needs no extra dependency, and
//! the arguments are passed as an argv array — never a shell string — so there
//! is no command-injection surface even though the Codex account name is
//! derived from a path.
//!
//! `security(1)` reference:
//! <https://ss64.com/osx/security.html> (`find-generic-password`).

/// Read a generic-password value (`security find-generic-password … -w`) for
/// `service` and an optional `account`. Returns `None` when the item is
/// absent, the `security` binary can't be run, the output isn't UTF-8, or the
/// build target isn't macOS.
#[cfg(target_os = "macos")]
pub(crate) fn read_generic_password(service: &str, account: Option<&str>) -> Option<String> {
    let mut cmd = std::process::Command::new("security");
    cmd.arg("find-generic-password").arg("-s").arg(service);
    if let Some(account) = account {
        cmd.arg("-a").arg(account);
    }
    cmd.arg("-w");
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Non-macOS stub — bitrouter only reads the macOS Keychain; other platforms
/// fall back to the vendor CLI's on-disk credential file.
#[cfg(not(target_os = "macos"))]
pub(crate) fn read_generic_password(_service: &str, _account: Option<&str>) -> Option<String> {
    None
}
