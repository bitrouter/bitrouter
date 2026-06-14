//! Generate a per-worker `opencode.json` and the env/cwd to launch it with.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::json;

use bitrouter_sdk::{BitrouterError, Result};

/// A materialized worker workspace: a temp dir holding `opencode.json`, the env
/// (`OPENCODE_CONFIG`) to launch with, and the `--cwd` to pass. Dropping it
/// removes the temp dir.
pub struct WorkerWorkspace {
    /// Temp root (config + worktree). Removed on drop.
    pub root: PathBuf,
    /// Env to merge into the spawn (`OPENCODE_CONFIG`).
    pub env: BTreeMap<String, String>,
    /// Absolute cwd to pass as `--cwd`.
    pub cwd: String,
}

impl Drop for WorkerWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Create an isolated temp worktree for one subagent run: `<tmp>/bitrouter-subagent-<uniq>/ws`.
/// Returns (root, cwd). The caller owns cleanup (e.g. via `WorkerWorkspace`'s Drop).
pub fn make_worktree(unique: &str) -> Result<(std::path::PathBuf, String)> {
    let root = std::env::temp_dir().join(format!("bitrouter-subagent-{unique}"));
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws)
        .map_err(|e| BitrouterError::internal(format!("mkdir {}: {e}", ws.display())))?;
    Ok((root, ws.to_string_lossy().to_string()))
}

/// The WIRE model id the daemon (and `PolicyHook`) sees — the part after the
/// first `/` (fallback: the whole string). Both the policy allowlist and a
/// harness's model pin MUST use this so they never diverge (else every worker
/// call is denied as `ModelNotAllowed`).
pub fn wire_model_id(model: &str) -> &str {
    model.split_once('/').map(|(_, m)| m).unwrap_or(model)
}

/// The model id as opencode expects it: `provider/model`. We always route via a
/// provider named `bitrouter` pointed at the daemon, so split on the first `/`.
fn split_provider_model(model: &str) -> Result<(&str, &str)> {
    model.split_once('/').ok_or_else(|| {
        BitrouterError::bad_request(format!("model '{model}' must be 'provider/model'"))
    })
}

/// Write `opencode.json` pinning `model` to a `bitrouter` provider at `base_url`
/// authenticated with `brvk_secret`. `unique` disambiguates the temp dir.
pub fn materialize(
    base_url: &str,
    model: &str,
    brvk_secret: &str,
    unique: &str,
) -> Result<WorkerWorkspace> {
    let (_provider, model_id) = split_provider_model(model)?;
    let (root, cwd) = make_worktree(unique)?;

    let cfg = json!({
        "$schema": "https://opencode.ai/config.json",
        "model": format!("bitrouter/{model_id}"),
        "permission": { "*": "allow" },
        "provider": {
            "bitrouter": {
                "name": "BitRouter",
                "npm": "@ai-sdk/openai-compatible",
                "models": { model_id: { "name": model_id } },
                "options": { "baseURL": base_url, "apiKey": brvk_secret }
            }
        }
    });
    let cfg_path = root.join("opencode.json");
    let cfg_bytes = serde_json::to_vec_pretty(&cfg)
        .map_err(|e| BitrouterError::internal(format!("serializing worker config: {e}")))?;
    std::fs::write(&cfg_path, cfg_bytes)
        .map_err(|e| BitrouterError::internal(format!("write {}: {e}", cfg_path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o600));
    }

    let mut env = BTreeMap::new();
    env.insert(
        "OPENCODE_CONFIG".to_string(),
        cfg_path.to_string_lossy().to_string(),
    );
    Ok(WorkerWorkspace { root, env, cwd })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_model_id_strips_first_segment() {
        assert_eq!(wire_model_id("bitrouter/z-ai/glm-5.1"), "z-ai/glm-5.1");
        assert_eq!(wire_model_id("bitrouter/kimi-k2.6"), "kimi-k2.6");
        assert_eq!(wire_model_id("m1"), "m1");
    }

    #[test]
    fn materialize_writes_pinned_config() {
        let ws = materialize(
            "http://127.0.0.1:4356/v1",
            "bitrouter/z-ai/glm-5.1",
            "brvk_secret_xyz",
            "test1",
        )
        .unwrap();
        let cfg_path = ws.root.join("opencode.json");
        let raw = std::fs::read_to_string(&cfg_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["model"], "bitrouter/z-ai/glm-5.1");
        assert_eq!(
            v["provider"]["bitrouter"]["options"]["baseURL"],
            "http://127.0.0.1:4356/v1"
        );
        assert_eq!(
            v["provider"]["bitrouter"]["options"]["apiKey"],
            "brvk_secret_xyz"
        );
        assert_eq!(v["permission"]["*"], "allow");
        assert!(ws.env.contains_key("OPENCODE_CONFIG"));
        let root = ws.root.clone();
        drop(ws);
        assert!(!root.exists(), "workspace removed on drop");
    }
}
