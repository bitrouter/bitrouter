use bitrouter_ai::harness::resources::MaterialRef;
use bitrouter_ai::harness::skills::{LocalSkill, LocalSkillCatalog};

use std::path::Path;

use crate::gateway::caller::CallerContext;
use async_trait::async_trait;

use crate::storage::checkpoint::sha256;

pub trait LocalSkillExt: Sized {
    fn material(&self) -> Result<MaterialRef, String>;
}

impl LocalSkillExt for LocalSkill {
    fn material(&self) -> Result<MaterialRef, String> {
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

pub(crate) fn canonical(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: std::collections::BTreeMap<_, _> = map
                .into_iter()
                .map(|(key, value)| (key, canonical(value)))
                .collect();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical).collect())
        }
        other => other,
    }
}
