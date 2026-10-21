//! Read-only adapter to the product's existing versioned installation store.
//! The installer owns receipts and publication; native hosts retain bindings.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bitrouter_sdk::caller::CallerContext;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::{MaterialRef, canonical, sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LibraryScope {
    Project,
    Global,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedSkill {
    pub scope: LibraryScope,
    pub library: PathBuf,
    pub workspace: PathBuf,
    pub api_key_id: String,
    pub user_id: String,
    pub local: bool,
    pub installation_id: String,
    pub catalog_revision: u64,
    pub name: String,
    pub description: String,
    /// Whole-bundle version, never just the SKILL.md digest.
    pub version: String,
    /// Exact original installer receipt, including full manifest and provenance.
    pub receipt: Value,
    pub instruction_sha256: String,
    pub instruction_bytes: u64,
}

impl ManagedSkill {
    pub fn material(&self) -> Result<MaterialRef, String> {
        let identity = serde_json::json!([
            self.scope,
            self.library,
            self.workspace,
            self.api_key_id,
            self.user_id,
            self.local,
            self.installation_id,
            self.version,
            self.receipt
        ]);
        let identity =
            serde_json::to_vec(&canonical(identity)).map_err(|error| error.to_string())?;
        Ok(MaterialRef {
            material_id: format!("managed_skill_{}", sha256(&identity)),
            version: self.version.clone(),
            sha256: self.instruction_sha256.clone(),
            media_type: "text/markdown".into(),
            provenance: format!("managed_skill:{:?}:{}", self.scope, self.installation_id),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedSkillCatalog {
    pub skills: Vec<ManagedSkill>,
    pub problems: Vec<String>,
    #[serde(default)]
    pub revisions: Vec<LibraryRevision>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LibraryRevision {
    pub scope: LibraryScope,
    pub library: PathBuf,
    pub revision: u64,
}

#[async_trait]
pub trait SkillLibrary: Send + Sync {
    /// Alias snapshot only. Uninstalled retained versions are not new candidates.
    async fn catalog(
        &self,
        workspace: &Path,
        caller: &CallerContext,
    ) -> Result<ManagedSkillCatalog, String>;

    /// Resolve the original receipt even after its alias changes/disappears.
    /// Each read rechecks authority, containment and the exact file's raw bytes.
    async fn read(
        &self,
        workspace: &Path,
        caller: &CallerContext,
        binding: &ManagedSkill,
        relative_path: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, String>;
}
