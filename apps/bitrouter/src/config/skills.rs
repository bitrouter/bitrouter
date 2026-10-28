//! Host-owned native skill source policy, independent of config-file parsing.
//! Ordinary MCP configuration does
//! not grant access to a server's skills or authorize skill-induced effects.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Native host source policy. Source permission is separate from per-skill consent.
pub struct SkillsConfig {
    /// Keys are host configuration IDs from `mcp_servers`, never serverInfo names.
    pub mcp_sources: BTreeMap<String, McpSkillSourceGrant>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Caller and workspace scopes allowed to discover/load one configured source.
pub struct McpSkillSourceGrant {
    /// Exact workspace roots. Subdirectories do not inherit this source grant.
    pub workspaces: Vec<PathBuf>,
    /// Permit the synthetic local caller for these workspaces.
    pub local: bool,
    /// Authenticated caller key IDs, not bearer credentials.
    pub api_key_ids: Vec<String>,
    /// Authenticated caller user IDs.
    pub user_ids: Vec<String>,
}

impl McpSkillSourceGrant {
    /// Validate policy declarations without resolving paths or contacting MCP.
    pub fn validate(&self) -> Result<(), String> {
        if self.workspaces.is_empty()
            || self.workspaces.len() > 128
            || self.workspaces.iter().any(|path| !path.is_absolute())
            || self.api_key_ids.len() > 256
            || self.user_ids.len() > 256
            || self
                .api_key_ids
                .iter()
                .chain(&self.user_ids)
                .any(|id| id.is_empty())
        {
            return Err(
                "skill source needs bounded exact absolute workspaces and nonempty caller IDs"
                    .into(),
            );
        }
        Ok(())
    }
}
