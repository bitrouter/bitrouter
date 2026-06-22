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

/// Parse the account (`"acct"`) attribute out of `security find-generic-password
/// -s <service>` attribute output. The attribute dump prints one
/// `"key"<blob>="value"` line per attribute; the generic-password item Claude
/// Code writes carries the macOS login account name under `acct`. Pure so it is
/// testable without touching a real Keychain.
///
/// macOS-only: it parses `security(1)` output and is only called by
/// [`find_account`], so compiling it elsewhere would be dead code.
#[cfg(target_os = "macos")]
pub(crate) fn parse_account(attributes_output: &str) -> Option<String> {
    for line in attributes_output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("\"acct\"<blob>=\"")
            && let Some(value) = rest.strip_suffix('"')
        {
            return Some(value.to_string());
        }
    }
    None
}

/// Discover the account name of the generic-password item stored under
/// `service`. Needed for an upsert write-back: a Keychain generic password is
/// keyed by `(service, account)`, so writing without the original account would
/// create a *second* item rather than update Claude Code's. Returns `None` when
/// the item is absent or the platform isn't macOS.
#[cfg(target_os = "macos")]
pub(crate) fn find_account(service: &str) -> Option<String> {
    // `find-generic-password -s <service>` (no `-a`) returns the *first*
    // matching item, so this assumes a single item per service — true for
    // Claude Code, which writes exactly one `Claude Code-credentials` item.
    // Without `-w` the command prints the item's attributes (incl. `acct`).
    let output = std::process::Command::new("security")
        .arg("find-generic-password")
        .arg("-s")
        .arg(service)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // `security` prints the attribute dump to stdout; some versions mirror it
    // to stderr. Check both.
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_account(&stdout).or_else(|| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        parse_account(&stderr)
    })
}

/// Non-macOS stub.
#[cfg(not(target_os = "macos"))]
pub(crate) fn find_account(_service: &str) -> Option<String> {
    None
}

/// Upsert a generic-password value for `(service, account)`. `-U` updates the
/// existing item in place (or creates it if absent) — exactly the write-back
/// semantics needed so Claude Code's own item is updated rather than shadowed
/// by a duplicate. Returns `true` on success.
///
/// `security(1)` reference:
/// <https://ss64.com/osx/security.html> (`add-generic-password -U`).
#[cfg(target_os = "macos")]
pub(crate) fn write_generic_password(service: &str, account: &str, value: &str) -> bool {
    std::process::Command::new("security")
        .arg("add-generic-password")
        .arg("-U")
        .arg("-s")
        .arg(service)
        .arg("-a")
        .arg(account)
        .arg("-w")
        .arg(value)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Non-macOS stub — write-back targets the on-disk credential file there.
#[cfg(not(target_os = "macos"))]
pub(crate) fn write_generic_password(_service: &str, _account: &str, _value: &str) -> bool {
    false
}

// macOS-only: the only tests here exercise the macOS-gated `parse_account`.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn parse_account_extracts_acct_field() {
        let out = "keychain: \"/Users/x/Library/Keychains/login.keychain-db\"\n    \
                   0x00000007 <blob>=\"Claude Code-credentials\"\n    \
                   \"acct\"<blob>=\"archer\"\n    \
                   \"svce\"<blob>=\"Claude Code-credentials\"\n";
        assert_eq!(parse_account(out).as_deref(), Some("archer"));
    }

    #[test]
    fn parse_account_none_when_absent() {
        assert_eq!(parse_account("nothing here\n  \"svce\"<blob>=\"x\""), None);
    }
}
