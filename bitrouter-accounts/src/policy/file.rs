//! Policy file loading with tracing-based error reporting.
//!
//! Data types are defined in [`bitrouter_core::policy`] — this module
//! provides the I/O layer that loads policy files from disk.

use std::path::Path;

pub use bitrouter_core::policy::{
    PolicyConfig, PolicyContext, PolicyFile, PolicyResult, ToolProviderPolicy, policy_dir,
};

/// Load all policy files from the policy directory.
///
/// Malformed files are logged and skipped. Returns policies sorted by name.
pub fn load_policies(dir: &Path) -> Result<Vec<PolicyFile>, Box<dyn std::error::Error>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut policies = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<PolicyFile>(&content) {
                    Ok(pf) => policies.push(pf),
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "skipping malformed policy file",
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "cannot read policy file",
                    );
                }
            }
        }
    }

    policies.sort_by(|a, b| a.config.name.cmp(&b.config.name));
    Ok(policies)
}
