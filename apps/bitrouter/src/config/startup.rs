//! Native startup working budgets, independent of retained clean history.

use std::path::PathBuf;

use serde::Deserialize;

// Allow for spare hash buckets, control bytes and growth alongside the old
// allocation. Dynamic workspace/key allocations are charged separately.
pub(crate) const UNRESOLVED_ENTRY_OVERHEAD: usize =
    4 * std::mem::size_of::<(String, PathBuf)>() + 16;

/// Restart-only budgets for the shared native service, including local transports.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StartupConfig {
    /// Maximum unresolved execution roots retained to protect their workspaces.
    pub unresolved_roots: usize,
    /// Retained unresolved keys, workspace paths and map allocation allowance.
    pub unresolved_metadata_bytes: usize,
    /// Records per cooperative audit batch; exhaustion yields and continues.
    pub records_per_batch: u64,
}

impl Default for StartupConfig {
    fn default() -> Self {
        Self {
            unresolved_roots: 1024,
            unresolved_metadata_bytes: 4 * 1024 * 1024,
            records_per_batch: 1_000_000,
        }
    }
}

impl StartupConfig {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.unresolved_roots == 0
            || self.unresolved_metadata_bytes == 0
            || self.records_per_batch == 0
        {
            return Err("agent_api.startup budgets must be positive");
        }
        // No allocation is proportional to the record batch. Root and byte
        // budgets must have addressable accounting even when only one binds.
        if self
            .unresolved_roots
            .checked_mul(UNRESOLVED_ENTRY_OVERHEAD)
            .and_then(|bytes| bytes.checked_add(self.unresolved_metadata_bytes))
            .is_none_or(|bytes| bytes > isize::MAX as usize)
        {
            return Err("agent_api.startup metadata accounting exceeds addressable memory");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_overrides_apply_with_http_disabled() -> anyhow::Result<()> {
        let defaults = crate::config::parse_with("inherit_defaults: false\n", |_| None)?;
        assert_eq!(defaults.agent_api.startup, StartupConfig::default());
        let configured = crate::config::parse_with(
            "inherit_defaults: false\nagent_api:\n  enabled: false\n  startup:\n    unresolved_roots: 100001\n    unresolved_metadata_bytes: 67108865\n    records_per_batch: 10000001\n",
            |_| None,
        )?;
        assert!(!configured.agent_api.enabled);
        assert_eq!(configured.agent_api.startup.unresolved_roots, 100_001);
        assert_eq!(
            configured.agent_api.startup.unresolved_metadata_bytes,
            67_108_865
        );
        assert_eq!(configured.agent_api.startup.records_per_batch, 10_000_001);
        Ok(())
    }

    #[test]
    fn invalid_budgets_fail_without_allocating() {
        for setting in [
            "unresolved_roots: 0".to_string(),
            "unresolved_metadata_bytes: 0".to_string(),
            "records_per_batch: 0".to_string(),
            format!("unresolved_roots: {}", usize::MAX),
            format!("unresolved_metadata_bytes: {}", usize::MAX),
            "unknown_budget: 1".to_string(),
        ] {
            let yaml = format!("inherit_defaults: false\nagent_api:\n  startup:\n    {setting}\n");
            assert!(crate::config::parse_with(&yaml, |_| None).is_err());
        }
        assert!(
            StartupConfig {
                records_per_batch: u64::MAX,
                ..StartupConfig::default()
            }
            .validate()
            .is_ok()
        );
    }
}
