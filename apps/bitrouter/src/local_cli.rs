//! Optional local workers for the maintained ACP adapters. The spawned
//! protocol peer remains the adapter; a CLI is never an interactive harness.

use std::path::Path;
use std::time::Duration;

use bitrouter_sdk::acp::transport::AcpTransport;
use semver::Version;

/// Follow the adapters' explicit executable override contracts:
/// https://github.com/agentclientprotocol/codex-acp
/// https://github.com/agentclientprotocol/claude-agent-acp
/// Minimums track the CLI versions shipped by the pinned adapter dependencies
/// (@openai/codex and claude-agent-sdk's claudeCodeVersion respectively).
fn worker(id: &str) -> Option<(&'static str, &'static str, Version)> {
    match id {
        "codex-acp" => Some(("codex", "CODEX_PATH", Version::new(0, 153, 3))),
        "claude-acp" => Some(("claude", "CLAUDE_CODE_EXECUTABLE", Version::new(2, 1, 257))),
        _ => None,
    }
}

/// Respect explicit adapter and inherited overrides. Old, missing, broken or
/// unresponsive local CLIs leave the adapter's bundled worker in control.
pub(crate) async fn apply(transport: &mut AcpTransport) {
    let AcpTransport::Stdio { command, args, env } = transport;
    let Some(harness) = crate::harness::match_invocation(command, args) else {
        return;
    };
    if !harness.uses_maintained_adapter(command, args) {
        return;
    }
    let Some((binary, variable, minimum)) = worker(harness.id) else {
        return;
    };
    if env.contains_key(variable) || std::env::var_os(variable).is_some() {
        return;
    }
    let Some(path) = crate::spawn::resolve_binary(binary) else {
        return;
    };
    let Ok(path) = std::path::absolute(path) else {
        return;
    };
    if compatible(&path, &minimum, Duration::from_secs(2)).await {
        let Some(path) = path.to_str() else { return };
        env.insert(variable.to_string(), path.to_string());
        eprintln!("Using local {binary} via {}: {path}", harness.id);
    } else {
        eprintln!(
            "Local {binary} does not meet {minimum} or could not be checked; using the ACP adapter's bundled CLI."
        );
    }
}

fn version(output: &[u8]) -> Option<Version> {
    std::str::from_utf8(output)
        .ok()?
        .split_whitespace()
        .find_map(|word| Version::parse(word.trim_start_matches('v')).ok())
}

async fn compatible(path: &Path, minimum: &Version, timeout: Duration) -> bool {
    let mut command = tokio::process::Command::new(path);
    let output = command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let Ok(Ok(output)) = tokio::time::timeout(timeout, output).await else {
        return false;
    };
    output.status.success()
        && version(&output.stdout)
            .is_some_and(|version| version.pre.is_empty() && version >= *minimum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vendor_versions_without_accepting_arbitrary_output() {
        assert_eq!(
            version(b"codex-cli 0.153.4\n"),
            Some(Version::new(0, 153, 4))
        );
        assert_eq!(
            version(b"2.1.257 (Claude Code)\n"),
            Some(Version::new(2, 1, 257))
        );
        assert_eq!(version(b"please log in"), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probes_compatible_old_failed_and_hung_workers() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("worker");
        let minimum = Version::new(0, 153, 3);
        for (body, expected) in [
            ("printf 'codex-cli 0.153.4\\n'", true),
            ("printf 'codex-cli 0.153.3\\n'", true),
            ("printf 'codex-cli 0.148.0\\n'", false),
            ("printf 'codex-cli 0.154.0-beta.1\\n'", false),
            ("printf 'codex-cli 0.153.4\\n'; exit 1", false),
            ("exec sleep 5", false),
        ] {
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n"))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
            assert_eq!(
                compatible(&path, &minimum, Duration::from_secs(2)).await,
                expected,
                "{body}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn keeps_explicit_overrides_and_custom_adapters() -> anyhow::Result<()> {
        for (package, explicit) in [
            ("@agentclientprotocol/codex-acp@1.10.0", true),
            ("@agentclientprotocol/codex-acp@1.7.0", false),
        ] {
            let mut transport = AcpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), package.into()],
                env: if explicit {
                    [("CODEX_PATH".into(), "/custom/codex".into())].into()
                } else {
                    Default::default()
                },
            };
            let before = transport.clone();
            apply(&mut transport).await;
            assert_eq!(transport, before);
        }
        Ok(())
    }
}
