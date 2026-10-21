//! Read-only local-directory contract; acquisition belongs to external installers.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bitrouter_sdk::caller::CallerContext;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::{MaterialRef, canonical, sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalScope {
    Project,
    Global,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalSkill {
    pub scope: LocalScope,
    pub directory: PathBuf,
    pub workspace: PathBuf,
    pub api_key_id: String,
    pub user_id: String,
    pub local: bool,
    pub directory_id: String,
    pub name: String,
    pub description: String,
    /// Content fingerprint of the directory manifest, without a version store.
    pub version: String,
    /// Frozen file manifest captured from the existing skill directory.
    pub receipt: Value,
    pub instruction_sha256: String,
    pub instruction_bytes: u64,
}

impl LocalSkill {
    pub fn material(&self) -> Result<MaterialRef, String> {
        let identity = serde_json::json!([
            self.scope,
            self.directory,
            self.workspace,
            self.api_key_id,
            self.user_id,
            self.local,
            self.directory_id,
            self.version,
            self.receipt
        ]);
        let identity =
            serde_json::to_vec(&canonical(identity)).map_err(|error| error.to_string())?;
        Ok(MaterialRef {
            material_id: format!("local_skill_{}", sha256(&identity)),
            version: self.version.clone(),
            sha256: self.instruction_sha256.clone(),
            media_type: "text/markdown".into(),
            provenance: format!("local_skill:{:?}:{}", self.scope, self.directory_id),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalSkillCatalog {
    pub skills: Vec<LocalSkill>,
    pub problems: Vec<String>,
}

#[async_trait]
pub trait SkillDirectory: Send + Sync {
    /// Discover existing local skill directories; never download or publish content.
    async fn catalog(
        &self,
        workspace: &Path,
        caller: &CallerContext,
    ) -> Result<LocalSkillCatalog, String>;

    /// Read the original directory only while its file commitments still match.
    /// Each read rechecks authority, containment and the exact file's raw bytes.
    async fn read(
        &self,
        workspace: &Path,
        caller: &CallerContext,
        binding: &LocalSkill,
        relative_path: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, String>;
}
