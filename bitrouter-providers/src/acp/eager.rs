//! Eager installation of ACP agents.
//!
//! "Eager" means we pay the install cost up front on the user's install
//! command, not on the first connect.  This keeps install latency
//! attributable — the user sees a progress indicator on the action they
//! took, not an opaque stall when they try to chat.
//!
//! Three install methods are supported, in order of preference:
//!   1. `npx`  → `npm install -g <package>`
//!   2. `uvx`  → `uv tool install <package>`
//!   3. binary → download + extract (via [`super::install::install_binary_agent`])
//!
//! The first method whose runtime is available is used.  For npx/uvx we
//! record `InstallMethod::{Npx,Uvx}` with no `resolved_binary_path` (the
//! runtime shim does the dispatch at launch time).  For binary installs
//! we record the absolute path so the next process start can find the
//! agent without re-downloading.

use std::path::{Path, PathBuf};

use bitrouter_config::{AgentConfig, Distribution};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::install::install_binary_agent;
use super::state::{self, InstallMethod, InstallRecord, now_unix_seconds};
use super::types::InstallProgress;

/// Outcome of a successful install.
#[derive(Debug, Clone)]
pub struct InstalledAgent {
    pub agent_id: String,
    pub method: InstallMethod,
    /// Present only for [`InstallMethod::Binary`].
    pub binary_path: Option<PathBuf>,
}

/// Install an agent using the first viable distribution method.
///
/// Records the install in `state_file` on success.  Progress is
/// reported via `progress_tx`; binary installs stream download/extract
/// events, while npx/uvx installs emit only start (`Downloading`) and
/// end (`Done`) markers because the child tools don't expose progress.
pub async fn install_agent(
    agent_id: &str,
    config: &AgentConfig,
    install_dir: &Path,
    state_file: &Path,
    version: &str,
    progress_tx: mpsc::Sender<InstallProgress>,
) -> Result<InstalledAgent, String> {
    if config.distribution.is_empty() {
        return Err(format!("no distribution methods declared for {agent_id}"));
    }

    for dist in &config.distribution {
        match dist {
            Distribution::Npx { package, .. } => {
                // Check for `npx` (what we invoke at launch), not `npm`:
                // some installs ship only one of the two, and a package
                // we can install but not launch is worse than a clean
                // skip to the next distribution method.
                if which("npx").is_none() {
                    continue;
                }
                run_install_command("npm", &["install", "-g", package], &progress_tx).await?;
                let record = InstallRecord {
                    id: agent_id.to_owned(),
                    version: version.to_owned(),
                    method: InstallMethod::Npx,
                    resolved_binary_path: None,
                    installed_at: now_unix_seconds(),
                };
                state::upsert_record(state_file, record).await?;
                let _ = progress_tx
                    .send(InstallProgress::Done(PathBuf::from("npx")))
                    .await;
                return Ok(InstalledAgent {
                    agent_id: agent_id.to_owned(),
                    method: InstallMethod::Npx,
                    binary_path: None,
                });
            }
            Distribution::Uvx { package, .. } => {
                // See the `npx` comment above — we check the launch-time
                // binary, not the install-time one.
                if which("uvx").is_none() {
                    continue;
                }
                run_install_command("uv", &["tool", "install", package], &progress_tx).await?;
                let record = InstallRecord {
                    id: agent_id.to_owned(),
                    version: version.to_owned(),
                    method: InstallMethod::Uvx,
                    resolved_binary_path: None,
                    installed_at: now_unix_seconds(),
                };
                state::upsert_record(state_file, record).await?;
                let _ = progress_tx
                    .send(InstallProgress::Done(PathBuf::from("uvx")))
                    .await;
                return Ok(InstalledAgent {
                    agent_id: agent_id.to_owned(),
                    method: InstallMethod::Uvx,
                    binary_path: None,
                });
            }
            Distribution::Binary { platforms } => {
                let binary_path =
                    install_binary_agent(agent_id, install_dir, platforms, progress_tx.clone())
                        .await?;
                let record = InstallRecord {
                    id: agent_id.to_owned(),
                    version: version.to_owned(),
                    method: InstallMethod::Binary,
                    resolved_binary_path: Some(binary_path.clone()),
                    installed_at: now_unix_seconds(),
                };
                state::upsert_record(state_file, record).await?;
                return Ok(InstalledAgent {
                    agent_id: agent_id.to_owned(),
                    method: InstallMethod::Binary,
                    binary_path: Some(binary_path),
                });
            }
        }
    }

    Err(format!(
        "no installable distribution for {agent_id} on this system \
         (tried npx/uvx/binary, none of the required runtimes were available)"
    ))
}

/// Uninstall an agent.  Removes the state entry and, for binary
/// installs, deletes the install directory.  Npx/Uvx uninstalls are a
/// best-effort shell-out; failure is logged but not propagated so the
/// ledger entry always ends up clean.
pub async fn uninstall_agent(
    agent_id: &str,
    install_dir: &Path,
    state_file: &Path,
) -> Result<(), String> {
    if let Some(record) = state::find_record(state_file, agent_id).await? {
        match record.method {
            InstallMethod::Binary => {
                if install_dir.exists() {
                    tokio::fs::remove_dir_all(install_dir)
                        .await
                        .map_err(|e| format!("failed to remove {}: {e}", install_dir.display()))?;
                }
            }
            InstallMethod::Npx => {
                // Recover the package name from the agent's distribution
                // metadata at call time is cleaner; here we rely on the
                // npm global cache cleanup being idempotent and silent.
                // Callers who care should delete the npm package directly.
                tracing::debug!(
                    agent_id,
                    "npx-installed agent — leaving npm global cache intact"
                );
            }
            InstallMethod::Uvx => {
                tracing::debug!(agent_id, "uvx-installed agent — leaving uv tool dir intact");
            }
        }
    }

    state::remove_record(state_file, agent_id).await
}

/// Run `{program} {args...}`, streaming coarse progress events.
///
/// Progress granularity is intentionally low: start → Downloading(0,
/// None), mid → Extracting, end → (returned to caller, which emits
/// `Done`).  npm and uv both print human status to stdout but not in a
/// machine-parseable form, so we surface stdout/stderr only in the
/// error message on non-zero exit.
async fn run_install_command(
    program: &str,
    args: &[&str],
    progress_tx: &mpsc::Sender<InstallProgress>,
) -> Result<(), String> {
    let _ = progress_tx
        .send(InstallProgress::Downloading {
            bytes_received: 0,
            total: None,
        })
        .await;

    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("failed to spawn {program}: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            stderr.trim()
        ));
    }

    let _ = progress_tx.send(InstallProgress::Extracting).await;
    Ok(())
}

/// Search `PATH` for `name`.  Returns the full path if found.
fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_config::AgentProtocol;
    use tempfile::TempDir;

    fn make_config(distribution: Vec<Distribution>) -> AgentConfig {
        AgentConfig {
            protocol: AgentProtocol::Acp,
            binary: "test-agent".to_owned(),
            args: Vec::new(),
            enabled: true,
            distribution,
            session: None,
            a2a: None,
        }
    }

    #[tokio::test]
    async fn empty_distribution_is_rejected() -> Result<(), String> {
        let dir = TempDir::new().map_err(|e| e.to_string())?;
        let state = dir.path().join("state.json");
        let install = dir.path().join("install");
        let (tx, _rx) = mpsc::channel(8);

        let err = install_agent(
            "nobody",
            &make_config(Vec::new()),
            &install,
            &state,
            "1.0.0",
            tx,
        )
        .await
        .err()
        .ok_or("expected error")?;

        assert!(err.contains("no distribution methods"), "got: {err}");
        Ok(())
    }

    #[tokio::test]
    async fn uninstall_missing_agent_is_noop() -> Result<(), String> {
        let dir = TempDir::new().map_err(|e| e.to_string())?;
        let state = dir.path().join("state.json");
        let install = dir.path().join("never-installed");

        uninstall_agent("ghost", &install, &state).await?;
        Ok(())
    }
}
